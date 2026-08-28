use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};

use crate::api_cost::ApiCostAccumulator;
use crate::attribution::{estimate_call_weight, is_spark_model};
use crate::domain::{ApiCostAmount, TaskRecord, TokenUsage, UsageCall};

pub const UNKNOWN_PROJECT_KEY: &str = "unknown";
pub const UNKNOWN_PROJECT_LABEL: &str = "Unknown project";

/// A half-open UTC interval used by the summary view: `[starts_at, ends_at)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SummaryWindow {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

impl SummaryWindow {
    pub fn new(starts_at: DateTime<Utc>, ends_at: DateTime<Utc>) -> Option<Self> {
        (starts_at < ends_at).then_some(Self { starts_at, ends_at })
    }

    pub fn contains(self, timestamp: DateTime<Utc>) -> bool {
        timestamp >= self.starts_at && timestamp < self.ends_at
    }
}

/// One independently aggregatable usage sample.
///
/// Live rollout calls map one-to-one to samples. Persisted history may use a
/// coarser sample as long as its token, EST, API-cost and call-count fields are
/// already additive. Metadata from samples outside the selected window may be
/// used to complete the thread tree, but their usage is never counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummarySample {
    pub timestamp: DateTime<Utc>,
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    /// Optional opaque project identity supplied by persisted history. Raw
    /// rollout samples fall back to the canonical workspace path.
    pub project_key: Option<String>,
    /// Display-only project name kept separate from the grouping identity.
    pub project_label: Option<String>,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub source: Option<String>,
    pub token_usage: TokenUsage,
    pub estimated_cost_units: u128,
    pub api_long_context_extra_cost_units: u128,
    pub api_equivalent_cost: ApiCostAmount,
    pub call_count: u64,
}

impl SummarySample {
    fn metrics(&self) -> SummaryMetrics {
        SummaryMetrics {
            token_usage: self.token_usage,
            estimated_cost_units: self.estimated_cost_units,
            api_long_context_extra_cost_units: self.api_long_context_extra_cost_units,
            api_equivalent_cost: self.api_equivalent_cost,
            call_count: self.call_count,
        }
    }

    fn has_usage(&self) -> bool {
        !self.token_usage.is_zero()
            || self.estimated_cost_units > 0
            || self.api_long_context_extra_cost_units > 0
            || self.api_equivalent_cost.observed_samples > 0
            || self.api_equivalent_cost.observed_tokens > 0
            || self.call_count > 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SummaryMetrics {
    pub token_usage: TokenUsage,
    /// Base Codex credit-rate weight. This is additive, but is not itself an
    /// absolute server quota percentage for arbitrary time ranges.
    pub estimated_cost_units: u128,
    /// Optional API-style long-context surcharge stored separately so the UI
    /// can toggle Longx without rescanning rollout files.
    pub api_long_context_extra_cost_units: u128,
    pub api_equivalent_cost: ApiCostAmount,
    pub call_count: u64,
}

impl SummaryMetrics {
    pub fn add_assign(&mut self, other: Self) {
        self.token_usage.add_assign(other.token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(other.estimated_cost_units);
        self.api_long_context_extra_cost_units = self
            .api_long_context_extra_cost_units
            .saturating_add(other.api_long_context_extra_cost_units);
        self.api_equivalent_cost
            .add_assign(other.api_equivalent_cost);
        self.call_count = self.call_count.saturating_add(other.call_count);
    }

    pub fn estimated_units(self, api_long_context: bool) -> u128 {
        if api_long_context {
            self.estimated_cost_units
                .saturating_add(self.api_long_context_extra_cost_units)
        } else {
            self.estimated_cost_units
        }
    }

    pub fn token_share_percent(self, total: Self) -> f64 {
        percent(
            u128::from(self.token_usage.total_tokens),
            u128::from(total.token_usage.total_tokens),
        )
    }

    pub fn estimated_share_percent(self, total: Self, api_long_context: bool) -> f64 {
        percent(
            self.estimated_units(api_long_context),
            total.estimated_units(api_long_context),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailySummary {
    /// Calendar date in the offset supplied to [`summarize_samples`].
    pub date: NaiveDate,
    pub totals: SummaryMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HourlySummary {
    /// Start of the local wall-clock hour represented by this bucket.
    pub starts_at: NaiveDateTime,
    pub totals: SummaryMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub title: Option<String>,
    pub source: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Usage emitted by this thread only.
    pub own: SummaryMetrics,
    /// Usage emitted by this thread and all visible descendants in the same
    /// project. Collapsed tree rows should display this value.
    pub subtree: SummaryMetrics,
    pub children: Vec<ThreadSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSummary {
    /// Opaque persisted project id, or a canonical workspace path for raw
    /// rollout samples, used as the stable grouping identity.
    pub key: String,
    /// Workspace basename intended for display.
    pub label: String,
    pub cwd: Option<PathBuf>,
    pub totals: SummaryMetrics,
    pub days: Vec<DailySummary>,
    /// Sparse local wall-clock hourly totals used by the interactive Summary
    /// chart. Empty/unknown hours are materialized by the caller together with
    /// its independent history coverage metadata.
    pub hours: Vec<HourlySummary>,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageSummary {
    pub window: SummaryWindow,
    pub totals: SummaryMetrics,
    pub days: Vec<DailySummary>,
    pub hours: Vec<HourlySummary>,
    pub projects: Vec<ProjectSummary>,
}

#[derive(Clone, Debug, Default)]
struct ThreadMetadata {
    parent_thread_id: Option<String>,
    project_key: Option<String>,
    project_label: Option<String>,
    project_label_at: Option<DateTime<Utc>>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    source: Option<String>,
}

#[derive(Default)]
struct ProjectAccumulator {
    totals: SummaryMetrics,
    days: BTreeMap<NaiveDate, SummaryMetrics>,
    hours: BTreeMap<NaiveDateTime, SummaryMetrics>,
    label: Option<(DateTime<Utc>, String)>,
    cwd: Option<PathBuf>,
}

/// Aggregates already-additive samples into project totals, a thread tree and
/// calendar-day buckets.
///
/// Empty calendar days are materialized for stable axes. Callers backed by
/// partial history must pair these zero totals with coverage metadata so an
/// unknown day is rendered as a gap rather than as confirmed inactivity.
pub fn summarize_samples(
    samples: &[SummarySample],
    window: SummaryWindow,
    day_offset: FixedOffset,
) -> UsageSummary {
    summarize_samples_with_local_time(samples, window, |timestamp| {
        timestamp.with_timezone(&day_offset).naive_local()
    })
}

/// Variant used by the TUI so each timestamp is converted with the host's
/// real local offset. Unlike a single [`FixedOffset`], this remains correct
/// when a 30-day range crosses a daylight-saving transition.
pub fn summarize_samples_with_local_date(
    samples: &[SummarySample],
    window: SummaryWindow,
    local_date: impl Fn(DateTime<Utc>) -> NaiveDate,
) -> UsageSummary {
    summarize_samples_with_local_mapping(samples, window, local_date, |_| None)
}

/// Variant used by the TUI to retain a sparse one-hour series while resolving
/// project identity and thread ancestry in the same aggregation pass.
pub fn summarize_samples_with_local_time(
    samples: &[SummarySample],
    window: SummaryWindow,
    local_time: impl Fn(DateTime<Utc>) -> NaiveDateTime,
) -> UsageSummary {
    let local_time = &local_time;
    summarize_samples_with_local_mapping(
        samples,
        window,
        |timestamp| local_time(timestamp).date(),
        |timestamp| Some(local_hour_start(local_time(timestamp))),
    )
}

fn summarize_samples_with_local_mapping(
    samples: &[SummarySample],
    window: SummaryWindow,
    local_date: impl Fn(DateTime<Utc>) -> NaiveDate,
    local_hour: impl Fn(DateTime<Utc>) -> Option<NaiveDateTime>,
) -> UsageSummary {
    let mut metadata = HashMap::<String, ThreadMetadata>::new();
    let mut metadata_samples = samples.iter().collect::<Vec<_>>();
    metadata_samples.sort_by_key(|sample| sample.timestamp);
    for sample in metadata_samples {
        merge_metadata(&mut metadata, sample);
    }

    let mut resolved_cwds = HashMap::<String, Option<PathBuf>>::new();
    let thread_ids = metadata.keys().cloned().collect::<Vec<_>>();
    for thread_id in thread_ids {
        let mut visiting = HashSet::new();
        resolve_thread_cwd(&thread_id, &metadata, &mut resolved_cwds, &mut visiting);
    }
    let mut resolved_project_keys = HashMap::<String, String>::new();
    let mut resolved_project_labels = HashMap::<String, Option<(DateTime<Utc>, String)>>::new();
    for thread_id in metadata.keys() {
        let mut visiting = HashSet::new();
        resolve_thread_project_key(
            thread_id,
            &metadata,
            &resolved_cwds,
            &mut resolved_project_keys,
            &mut visiting,
        );
        let mut visiting = HashSet::new();
        resolve_thread_project_label(
            thread_id,
            &metadata,
            &resolved_cwds,
            &mut resolved_project_labels,
            &mut visiting,
        );
    }

    let dates = dates_in_window(window, &local_date);
    let mut totals = SummaryMetrics::default();
    let mut days = dates
        .iter()
        .copied()
        .map(|date| (date, SummaryMetrics::default()))
        .collect::<BTreeMap<_, _>>();
    let mut hours = BTreeMap::<NaiveDateTime, SummaryMetrics>::new();
    let mut projects = BTreeMap::<String, ProjectAccumulator>::new();
    let mut own_by_thread = HashMap::<String, SummaryMetrics>::new();

    for sample in samples
        .iter()
        .filter(|sample| sample.has_usage() && window.contains(sample.timestamp))
    {
        let metrics = sample.metrics();
        let date = local_date(sample.timestamp);
        let hour = local_hour(sample.timestamp);
        let cwd = resolved_cwds
            .get(&sample.thread_id)
            .cloned()
            .flatten()
            .or_else(|| sample.cwd.clone());
        let key = resolved_project_keys
            .get(&sample.thread_id)
            .cloned()
            .unwrap_or_else(|| project_key(cwd.as_deref()));

        totals.add_assign(metrics);
        days.entry(date).or_default().add_assign(metrics);
        if let Some(hour) = hour {
            hours.entry(hour).or_default().add_assign(metrics);
        }
        own_by_thread
            .entry(sample.thread_id.clone())
            .or_default()
            .add_assign(metrics);
        let project = projects.entry(key).or_default();
        project.totals.add_assign(metrics);
        project.days.entry(date).or_default().add_assign(metrics);
        if let Some(hour) = hour {
            project.hours.entry(hour).or_default().add_assign(metrics);
        }
        if project.cwd.is_none() {
            project.cwd = cwd;
        }
        if let Some((label_at, label)) = resolved_project_labels
            .get(&sample.thread_id)
            .and_then(|label| label.as_ref())
        {
            let replace = project
                .label
                .as_ref()
                .is_none_or(|(current_at, _)| label_at >= current_at);
            if replace {
                project.label = Some((*label_at, label.clone()));
            }
        }
    }

    let mut project_summaries = projects
        .into_iter()
        .map(|(key, accumulator)| {
            let ProjectAccumulator {
                totals,
                days: project_days,
                hours: project_hours,
                label,
                cwd,
            } = accumulator;
            let label = label
                .map(|(_, label)| label)
                .unwrap_or_else(|| project_label(cwd.as_deref()));
            let threads = build_project_threads(
                &key,
                &metadata,
                &resolved_cwds,
                &resolved_project_keys,
                &own_by_thread,
            );
            ProjectSummary {
                key,
                label,
                cwd,
                totals,
                days: dates
                    .iter()
                    .copied()
                    .map(|date| DailySummary {
                        date,
                        totals: project_days.get(&date).copied().unwrap_or_default(),
                    })
                    .collect(),
                hours: project_hours
                    .into_iter()
                    .map(|(starts_at, totals)| HourlySummary { starts_at, totals })
                    .collect(),
                threads,
            }
        })
        .collect::<Vec<_>>();
    project_summaries.sort_by(|left, right| {
        right
            .totals
            .token_usage
            .total_tokens
            .cmp(&left.totals.token_usage.total_tokens)
            .then_with(|| {
                right
                    .totals
                    .estimated_cost_units
                    .cmp(&left.totals.estimated_cost_units)
            })
            .then_with(|| left.key.cmp(&right.key))
    });

    UsageSummary {
        window,
        totals,
        days: days
            .into_iter()
            .map(|(date, totals)| DailySummary { date, totals })
            .collect(),
        hours: hours
            .into_iter()
            .map(|(starts_at, totals)| HourlySummary { starts_at, totals })
            .collect(),
        projects: project_summaries,
    }
}

fn local_hour_start(value: NaiveDateTime) -> NaiveDateTime {
    value
        .date()
        .and_hms_opt(value.hour(), 0, 0)
        .unwrap_or(value)
}

/// Adapts raw rollout calls to the same additive representation used by
/// persisted summary history.
///
/// As in the existing attribution and history paths, Spark is excluded from
/// local-token and EST totals while remaining visible to API-price coverage.
pub fn samples_from_calls(tasks: &[TaskRecord], calls: &[UsageCall]) -> Vec<SummarySample> {
    let tasks_by_thread = tasks
        .iter()
        .map(|task| (task.thread_id.as_str(), task))
        .collect::<HashMap<_, _>>();
    let mut samples = calls
        .iter()
        .map(|call| {
            let task = tasks_by_thread.get(call.thread_id.as_str()).copied();
            let mut api_cost = ApiCostAccumulator::default();
            api_cost.add_call(call);
            let (token_usage, estimated_cost_units, api_long_context_extra_cost_units) =
                if is_spark_model(call.model.as_deref()) {
                    (TokenUsage::default(), 0, 0)
                } else {
                    let estimated = estimate_call_weight(call);
                    (
                        call.tokens,
                        estimated.units,
                        estimated.api_long_context_extra_units,
                    )
                };
            SummarySample {
                timestamp: call.timestamp,
                thread_id: call.thread_id.clone(),
                parent_thread_id: task.and_then(|task| task.parent_thread_id.clone()),
                project_key: None,
                project_label: None,
                cwd: task.and_then(|task| task.cwd.clone()),
                title: task.map(|task| task.title.clone()),
                source: task.and_then(|task| task.source.clone()),
                token_usage,
                estimated_cost_units,
                api_long_context_extra_cost_units,
                api_equivalent_cost: api_cost.amount(),
                call_count: 1,
            }
        })
        .collect::<Vec<_>>();
    // Metadata-only samples keep zero-usage ancestors available to the tree.
    // Metadata is merged before zero-usage samples are filtered from metric
    // aggregation; newer non-empty fields supersede older session metadata.
    samples.extend(tasks.iter().map(|task| {
        SummarySample {
            timestamp: task
                .updated_at
                .or(task.created_at)
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
            thread_id: task.thread_id.clone(),
            parent_thread_id: task.parent_thread_id.clone(),
            project_key: None,
            project_label: None,
            cwd: task.cwd.clone(),
            title: Some(task.title.clone()),
            source: task.source.clone(),
            token_usage: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: 0,
            api_equivalent_cost: ApiCostAmount::default(),
            call_count: 0,
        }
    }));
    samples
}

pub fn summarize_calls_and_tasks(
    tasks: &[TaskRecord],
    calls: &[UsageCall],
    window: SummaryWindow,
    day_offset: FixedOffset,
) -> UsageSummary {
    summarize_samples(&samples_from_calls(tasks, calls), window, day_offset)
}

fn merge_metadata(metadata: &mut HashMap<String, ThreadMetadata>, sample: &SummarySample) {
    let entry = metadata.entry(sample.thread_id.clone()).or_default();
    if sample.parent_thread_id.is_some()
        && sample.parent_thread_id.as_deref() != Some(sample.thread_id.as_str())
    {
        entry.parent_thread_id = sample.parent_thread_id.clone();
    }
    if sample
        .project_key
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        entry.project_key = sample.project_key.clone();
    }
    if sample
        .project_label
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        entry.project_label = sample.project_label.clone();
        entry.project_label_at = Some(sample.timestamp);
    }
    if sample.cwd.is_some() {
        entry.cwd = sample.cwd.clone();
    }
    if sample
        .title
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        entry.title = sample.title.clone().filter(|value| !value.is_empty());
    }
    if sample
        .source
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        entry.source = sample.source.clone().filter(|value| !value.is_empty());
    }
}

fn resolve_thread_cwd(
    thread_id: &str,
    metadata: &HashMap<String, ThreadMetadata>,
    cache: &mut HashMap<String, Option<PathBuf>>,
    visiting: &mut HashSet<String>,
) -> Option<PathBuf> {
    if let Some(cached) = cache.get(thread_id) {
        return cached.clone();
    }
    if !visiting.insert(thread_id.to_string()) {
        return None;
    }
    let resolved = metadata.get(thread_id).and_then(|thread| {
        thread.cwd.clone().or_else(|| {
            thread
                .parent_thread_id
                .as_deref()
                .and_then(|parent| resolve_thread_cwd(parent, metadata, cache, visiting))
        })
    });
    visiting.remove(thread_id);
    cache.insert(thread_id.to_string(), resolved.clone());
    resolved
}

fn resolve_thread_project_key(
    thread_id: &str,
    metadata: &HashMap<String, ThreadMetadata>,
    resolved_cwds: &HashMap<String, Option<PathBuf>>,
    cache: &mut HashMap<String, String>,
    visiting: &mut HashSet<String>,
) -> String {
    if let Some(cached) = cache.get(thread_id) {
        return cached.clone();
    }
    if !visiting.insert(thread_id.to_string()) {
        return project_key(resolved_cwds.get(thread_id).and_then(|cwd| cwd.as_deref()));
    }
    let resolved = metadata
        .get(thread_id)
        .and_then(|thread| thread.project_key.clone())
        .or_else(|| {
            metadata
                .get(thread_id)
                .and_then(|thread| thread.parent_thread_id.as_deref())
                .map(|parent| {
                    resolve_thread_project_key(parent, metadata, resolved_cwds, cache, visiting)
                })
                .filter(|key| key != UNKNOWN_PROJECT_KEY)
        })
        .unwrap_or_else(|| {
            project_key(resolved_cwds.get(thread_id).and_then(|cwd| cwd.as_deref()))
        });
    visiting.remove(thread_id);
    cache.insert(thread_id.to_string(), resolved.clone());
    resolved
}

fn resolve_thread_project_label(
    thread_id: &str,
    metadata: &HashMap<String, ThreadMetadata>,
    resolved_cwds: &HashMap<String, Option<PathBuf>>,
    cache: &mut HashMap<String, Option<(DateTime<Utc>, String)>>,
    visiting: &mut HashSet<String>,
) -> Option<(DateTime<Utc>, String)> {
    if let Some(cached) = cache.get(thread_id) {
        return cached.clone();
    }
    if !visiting.insert(thread_id.to_string()) {
        return None;
    }
    let resolved = metadata
        .get(thread_id)
        .and_then(|thread| {
            thread
                .project_label
                .clone()
                .zip(thread.project_label_at)
                .map(|(label, at)| (at, label))
        })
        .or_else(|| {
            metadata
                .get(thread_id)
                .and_then(|thread| thread.parent_thread_id.as_deref())
                .and_then(|parent| {
                    resolve_thread_project_label(parent, metadata, resolved_cwds, cache, visiting)
                })
        })
        .or_else(|| {
            resolved_cwds
                .get(thread_id)
                .and_then(|cwd| cwd.as_deref())
                .map(|cwd| (DateTime::<Utc>::UNIX_EPOCH, project_label(Some(cwd))))
        });
    visiting.remove(thread_id);
    cache.insert(thread_id.to_string(), resolved.clone());
    resolved
}

fn build_project_threads(
    project_key: &str,
    metadata: &HashMap<String, ThreadMetadata>,
    resolved_cwds: &HashMap<String, Option<PathBuf>>,
    resolved_project_keys: &HashMap<String, String>,
    own_by_thread: &HashMap<String, SummaryMetrics>,
) -> Vec<ThreadSummary> {
    let mut included = BTreeSet::<String>::new();
    for thread_id in own_by_thread.keys() {
        if project_key_for_thread(thread_id, resolved_project_keys) != project_key {
            continue;
        }
        let mut current = Some(thread_id.as_str());
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.to_string())
                || project_key_for_thread(id, resolved_project_keys) != project_key
            {
                break;
            }
            included.insert(id.to_string());
            current = metadata
                .get(id)
                .and_then(|thread| thread.parent_thread_id.as_deref());
        }
    }

    let mut children = BTreeMap::<String, Vec<String>>::new();
    let mut roots = Vec::new();
    for thread_id in &included {
        let parent = metadata
            .get(thread_id)
            .and_then(|thread| thread.parent_thread_id.as_ref())
            .filter(|parent| included.contains(*parent))
            .filter(|parent| !parent_edge_would_cycle(thread_id, parent, metadata));
        if let Some(parent) = parent {
            children
                .entry(parent.clone())
                .or_default()
                .push(thread_id.clone());
        } else {
            roots.push(thread_id.clone());
        }
    }

    let mut built = roots
        .into_iter()
        .map(|thread_id| {
            build_thread_summary(
                &thread_id,
                metadata,
                resolved_cwds,
                own_by_thread,
                &children,
            )
        })
        .collect::<Vec<_>>();
    sort_thread_summaries(&mut built);
    built
}

fn build_thread_summary(
    thread_id: &str,
    metadata: &HashMap<String, ThreadMetadata>,
    resolved_cwds: &HashMap<String, Option<PathBuf>>,
    own_by_thread: &HashMap<String, SummaryMetrics>,
    children_by_thread: &BTreeMap<String, Vec<String>>,
) -> ThreadSummary {
    let mut children = children_by_thread
        .get(thread_id)
        .into_iter()
        .flatten()
        .map(|child| {
            build_thread_summary(
                child,
                metadata,
                resolved_cwds,
                own_by_thread,
                children_by_thread,
            )
        })
        .collect::<Vec<_>>();
    sort_thread_summaries(&mut children);

    let own = own_by_thread.get(thread_id).copied().unwrap_or_default();
    let mut subtree = own;
    for child in &children {
        subtree.add_assign(child.subtree);
    }
    let thread = metadata.get(thread_id).cloned().unwrap_or_default();
    ThreadSummary {
        thread_id: thread_id.to_string(),
        parent_thread_id: thread.parent_thread_id,
        title: thread.title,
        source: thread.source,
        cwd: resolved_cwds.get(thread_id).cloned().flatten(),
        own,
        subtree,
        children,
    }
}

fn sort_thread_summaries(threads: &mut [ThreadSummary]) {
    threads.sort_by(|left, right| {
        right
            .subtree
            .token_usage
            .total_tokens
            .cmp(&left.subtree.token_usage.total_tokens)
            .then_with(|| {
                right
                    .subtree
                    .estimated_cost_units
                    .cmp(&left.subtree.estimated_cost_units)
            })
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
}

fn parent_edge_would_cycle(
    child_id: &str,
    parent_id: &str,
    metadata: &HashMap<String, ThreadMetadata>,
) -> bool {
    let mut current = Some(parent_id);
    let mut seen = HashSet::new();
    while let Some(thread_id) = current {
        if thread_id == child_id || !seen.insert(thread_id) {
            return true;
        }
        current = metadata
            .get(thread_id)
            .and_then(|thread| thread.parent_thread_id.as_deref());
    }
    false
}

fn project_key_for_thread<'a>(
    thread_id: &str,
    resolved_project_keys: &'a HashMap<String, String>,
) -> &'a str {
    resolved_project_keys
        .get(thread_id)
        .map(String::as_str)
        .unwrap_or(UNKNOWN_PROJECT_KEY)
}

fn project_key(cwd: Option<&Path>) -> String {
    cwd.map(|cwd| cwd.to_string_lossy().into_owned())
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(|| UNKNOWN_PROJECT_KEY.to_string())
}

fn project_label(cwd: Option<&Path>) -> String {
    cwd.and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| UNKNOWN_PROJECT_LABEL.to_string())
}

fn dates_in_window(
    window: SummaryWindow,
    local_date: &impl Fn(DateTime<Utc>) -> NaiveDate,
) -> Vec<NaiveDate> {
    if window.starts_at >= window.ends_at {
        return Vec::new();
    }
    let first = local_date(window.starts_at);
    let last = local_date(window.ends_at - Duration::nanoseconds(1));
    let mut dates = Vec::new();
    let mut date = first;
    loop {
        dates.push(date);
        if date >= last {
            break;
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    dates
}

fn percent(value: u128, total: u128) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::{Confidence, Provenance, TaskStatus};

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn metrics(total_tokens: u64, estimated: u128, api: u128) -> SummaryMetrics {
        SummaryMetrics {
            token_usage: TokenUsage {
                input_tokens: total_tokens,
                total_tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units: estimated,
            api_equivalent_cost: ApiCostAmount {
                minimum_pico_usd: crate::domain::PicoUsd::new(api),
                maximum_pico_usd: crate::domain::PicoUsd::new(api),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: total_tokens,
                priced_tokens: total_tokens,
            },
            call_count: 1,
            ..SummaryMetrics::default()
        }
    }

    fn sample(
        timestamp: DateTime<Utc>,
        thread_id: &str,
        parent_thread_id: Option<&str>,
        cwd: Option<&str>,
        metrics: SummaryMetrics,
    ) -> SummarySample {
        SummarySample {
            timestamp,
            thread_id: thread_id.to_string(),
            parent_thread_id: parent_thread_id.map(str::to_string),
            project_key: None,
            project_label: None,
            cwd: cwd.map(PathBuf::from),
            title: Some(format!("title {thread_id}")),
            source: None,
            token_usage: metrics.token_usage,
            estimated_cost_units: metrics.estimated_cost_units,
            api_long_context_extra_cost_units: metrics.api_long_context_extra_cost_units,
            api_equivalent_cost: metrics.api_equivalent_cost,
            call_count: metrics.call_count,
        }
    }

    fn summed_metrics(values: impl IntoIterator<Item = SummaryMetrics>) -> SummaryMetrics {
        values
            .into_iter()
            .fold(SummaryMetrics::default(), |mut total, value| {
                total.add_assign(value);
                total
            })
    }

    #[test]
    fn aggregates_projects_and_collapsed_thread_subtrees_without_double_counting() {
        let window = SummaryWindow::new(at(1, 0), at(3, 0)).unwrap();
        let samples = vec![
            // Metadata-only parent retained even though its timestamp is out of range.
            sample(
                at(1, 0) - Duration::days(10),
                "root",
                None,
                Some("/work/alpha"),
                SummaryMetrics::default(),
            ),
            sample(
                at(1, 3),
                "child",
                Some("root"),
                None,
                metrics(30, 300, 3_000),
            ),
            sample(
                at(2, 3),
                "root",
                None,
                Some("/work/alpha"),
                metrics(20, 200, 2_000),
            ),
            sample(
                at(2, 4),
                "beta",
                None,
                Some("/work/beta"),
                metrics(10, 400, 1_000),
            ),
        ];

        let summary = summarize_samples(&samples, window, FixedOffset::east_opt(0).unwrap());

        assert_eq!(summary.totals.token_usage.total_tokens, 60);
        assert_eq!(summary.totals.estimated_cost_units, 900);
        assert_eq!(summary.projects.len(), 2);
        assert_eq!(summary.projects[0].key, "/work/alpha");
        assert_eq!(summary.projects[0].totals.token_usage.total_tokens, 50);
        let root = &summary.projects[0].threads[0];
        assert_eq!(root.thread_id, "root");
        assert_eq!(root.own.token_usage.total_tokens, 20);
        assert_eq!(root.subtree.token_usage.total_tokens, 50);
        assert_eq!(root.children[0].thread_id, "child");
        assert_eq!(root.children[0].subtree.token_usage.total_tokens, 30);
        assert_eq!(
            root.subtree.api_equivalent_cost.minimum_pico_usd.value(),
            5_000
        );
    }

    #[test]
    fn uses_half_open_window_and_materializes_empty_local_days() {
        let window = SummaryWindow::new(at(1, 16), at(3, 16)).unwrap();
        let offset = FixedOffset::east_opt(8 * 60 * 60).unwrap();
        let samples = vec![
            sample(
                at(1, 16),
                "included",
                None,
                Some("/work/a"),
                metrics(1, 1, 1),
            ),
            sample(
                at(3, 16),
                "excluded",
                None,
                Some("/work/a"),
                metrics(100, 100, 100),
            ),
        ];

        let summary = summarize_samples(&samples, window, offset);

        assert_eq!(summary.totals.token_usage.total_tokens, 1);
        assert_eq!(summary.days.len(), 2);
        assert_eq!(summary.days[0].date.to_string(), "2026-08-02");
        assert_eq!(summary.days[0].totals.token_usage.total_tokens, 1);
        assert_eq!(summary.days[1].date.to_string(), "2026-08-03");
        assert_eq!(summary.days[1].totals.token_usage.total_tokens, 0);
        assert_eq!(summary.projects[0].days.len(), 2);
    }

    #[test]
    fn aggregates_quarter_hour_samples_into_sparse_local_hours_and_preserves_totals() {
        let starts_at = at(1, 0);
        let window = SummaryWindow::new(starts_at, at(1, 2)).unwrap();
        let samples = vec![
            sample(
                starts_at,
                "alpha-1",
                None,
                Some("/work/alpha"),
                metrics(10, 100, 1_000),
            ),
            sample(
                starts_at + Duration::minutes(15),
                "alpha-2",
                None,
                Some("/work/alpha"),
                metrics(20, 200, 2_000),
            ),
            sample(
                starts_at + Duration::minutes(45),
                "beta-1",
                None,
                Some("/work/beta"),
                metrics(5, 50, 500),
            ),
            sample(
                starts_at + Duration::hours(1),
                "alpha-3",
                None,
                Some("/work/alpha"),
                metrics(7, 70, 700),
            ),
            sample(
                starts_at + Duration::hours(1) + Duration::minutes(15),
                "beta-2",
                None,
                Some("/work/beta"),
                metrics(3, 30, 300),
            ),
        ];

        let summary =
            summarize_samples_with_local_time(&samples, window, |timestamp| timestamp.naive_utc());

        assert_eq!(
            summary
                .hours
                .iter()
                .map(|hour| (hour.starts_at, hour.totals.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            [
                (starts_at.naive_utc(), 35),
                ((starts_at + Duration::hours(1)).naive_utc(), 10),
            ]
        );
        assert_eq!(
            summary.totals,
            summed_metrics(summary.hours.iter().map(|hour| hour.totals))
        );
        assert_eq!(
            summary.totals,
            summed_metrics(summary.projects.iter().map(|project| project.totals))
        );

        let alpha = summary
            .projects
            .iter()
            .find(|project| project.key == "/work/alpha")
            .unwrap();
        let beta = summary
            .projects
            .iter()
            .find(|project| project.key == "/work/beta")
            .unwrap();
        assert_eq!(
            alpha
                .hours
                .iter()
                .map(|hour| (hour.starts_at, hour.totals.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            [
                (starts_at.naive_utc(), 30),
                ((starts_at + Duration::hours(1)).naive_utc(), 7),
            ]
        );
        assert_eq!(
            beta.hours
                .iter()
                .map(|hour| (hour.starts_at, hour.totals.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            [
                (starts_at.naive_utc(), 5),
                ((starts_at + Duration::hours(1)).naive_utc(), 3),
            ]
        );
        for project in &summary.projects {
            assert_eq!(
                project.totals,
                summed_metrics(project.hours.iter().map(|hour| hour.totals))
            );
        }
    }

    #[test]
    fn local_hour_mapper_merges_the_repeated_fallback_hour_without_losing_usage() {
        let starts_at = Utc.with_ymd_and_hms(2026, 11, 1, 4, 0, 0).single().unwrap();
        let transition = Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).single().unwrap();
        let ends_at = Utc.with_ymd_and_hms(2026, 11, 1, 8, 0, 0).single().unwrap();
        let samples = [
            (Duration::minutes(15), 10),
            (Duration::hours(1) + Duration::minutes(15), 20),
            (Duration::hours(2) + Duration::minutes(15), 30),
            (Duration::hours(3) + Duration::minutes(15), 40),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (offset, tokens))| {
            sample(
                starts_at + offset,
                &format!("thread-{index}"),
                None,
                Some("/work/alpha"),
                metrics(tokens, u128::from(tokens), u128::from(tokens)),
            )
        })
        .collect::<Vec<_>>();

        let summary = summarize_samples_with_local_time(
            &samples,
            SummaryWindow::new(starts_at, ends_at).unwrap(),
            |timestamp| {
                let offset_hours = if timestamp < transition { -4 } else { -5 };
                (timestamp + Duration::hours(offset_hours)).naive_utc()
            },
        );
        let local_day = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let local_hour = |hour| local_day.and_hms_opt(hour, 0, 0).unwrap();

        assert_eq!(
            summary
                .hours
                .iter()
                .map(|hour| (hour.starts_at, hour.totals.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            [
                (local_hour(0), 10),
                (local_hour(1), 50),
                (local_hour(2), 40),
            ]
        );
        assert_eq!(summary.projects.len(), 1);
        assert_eq!(summary.projects[0].hours, summary.hours);
        assert_eq!(
            summary.totals,
            summed_metrics(summary.hours.iter().map(|hour| hour.totals))
        );
        assert_eq!(summary.totals, summary.projects[0].totals);
    }

    #[test]
    fn local_day_mapper_handles_an_offset_change_inside_the_window() {
        let starts_at = Utc.with_ymd_and_hms(2026, 3, 8, 4, 0, 0).single().unwrap();
        let transition = Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).single().unwrap();
        let ends_at = Utc.with_ymd_and_hms(2026, 3, 10, 4, 0, 0).single().unwrap();
        let samples = vec![
            sample(
                starts_at + Duration::minutes(30),
                "before",
                None,
                Some("/work/a"),
                metrics(1, 1, 1),
            ),
            sample(
                transition + Duration::minutes(30),
                "after",
                None,
                Some("/work/a"),
                metrics(2, 2, 2),
            ),
        ];

        let summary = summarize_samples_with_local_date(
            &samples,
            SummaryWindow::new(starts_at, ends_at).unwrap(),
            |timestamp| {
                let hours = if timestamp < transition { -5 } else { -4 };
                (timestamp + Duration::hours(hours)).date_naive()
            },
        );

        assert_eq!(
            summary
                .days
                .iter()
                .map(|day| (day.date.to_string(), day.totals.token_usage.total_tokens))
                .collect::<Vec<_>>(),
            [
                ("2026-03-07".to_string(), 1),
                ("2026-03-08".to_string(), 2),
                ("2026-03-09".to_string(), 0)
            ]
        );
    }

    #[test]
    fn keeps_same_basename_workspaces_as_distinct_projects() {
        let window = SummaryWindow::new(at(1, 0), at(2, 0)).unwrap();
        let samples = vec![
            sample(at(1, 1), "a", None, Some("/one/repo"), metrics(1, 1, 1)),
            sample(at(1, 2), "b", None, Some("/two/repo"), metrics(2, 2, 2)),
        ];

        let summary = summarize_samples(&samples, window, FixedOffset::east_opt(0).unwrap());

        assert_eq!(summary.projects.len(), 2);
        assert!(
            summary
                .projects
                .iter()
                .all(|project| project.label == "repo")
        );
        assert_ne!(summary.projects[0].key, summary.projects[1].key);
    }

    #[test]
    fn newest_metadata_wins_even_when_samples_are_not_in_timestamp_order() {
        let window = SummaryWindow::new(at(2, 0), at(3, 0)).unwrap();
        let mut current = sample(
            at(2, 1),
            "thread",
            None,
            Some("/work/current"),
            metrics(10, 10, 10),
        );
        current.title = Some("Current title".to_string());
        let mut old = sample(
            at(1, 1),
            "thread",
            None,
            Some("/work/old"),
            SummaryMetrics::default(),
        );
        old.title = Some("Untitled task".to_string());

        let summary = summarize_samples(&[current, old], window, FixedOffset::east_opt(0).unwrap());

        assert_eq!(summary.projects.len(), 1);
        assert_eq!(summary.projects[0].key, "/work/current");
        assert_eq!(summary.projects[0].label, "current");
        assert_eq!(
            summary.projects[0].threads[0].title.as_deref(),
            Some("Current title")
        );
    }

    #[test]
    fn opaque_project_id_is_identity_and_label_is_display_only() {
        let window = SummaryWindow::new(at(1, 0), at(2, 0)).unwrap();
        let mut first = sample(at(1, 1), "a", None, None, metrics(1, 1, 1));
        first.project_key = Some("stable-id".to_string());
        first.project_label = Some("old-name".to_string());
        let mut renamed = sample(at(1, 2), "b", None, None, metrics(2, 2, 2));
        renamed.project_key = Some("stable-id".to_string());
        renamed.project_label = Some("new-name".to_string());
        let mut distinct = sample(at(1, 3), "c", None, None, metrics(3, 3, 3));
        distinct.project_key = Some("another-id".to_string());
        distinct.project_label = Some("new-name".to_string());

        let summary = summarize_samples(
            &[distinct, renamed, first],
            window,
            FixedOffset::east_opt(0).unwrap(),
        );

        assert_eq!(summary.projects.len(), 2);
        let merged = summary
            .projects
            .iter()
            .find(|project| project.key == "stable-id")
            .unwrap();
        assert_eq!(merged.label, "new-name");
        assert_eq!(merged.totals.token_usage.total_tokens, 3);
        assert_eq!(merged.threads.len(), 2);
        assert!(
            summary
                .projects
                .iter()
                .any(|project| project.key == "another-id" && project.label == "new-name")
        );
    }

    #[test]
    fn call_adapter_reuses_estimator_and_api_pricer() {
        let timestamp = at(1, 1);
        let tasks = vec![TaskRecord {
            thread_id: "thread".to_string(),
            parent_thread_id: None,
            archived: false,
            title: "Task".to_string(),
            cwd: Some(PathBuf::from("/work/repo")),
            source: Some("cli".to_string()),
            created_at: Some(timestamp),
            updated_at: Some(timestamp),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: None,
        }];
        let calls = vec![UsageCall {
            timestamp,
            thread_id: "thread".to_string(),
            turn_id: Some("turn".to_string()),
            model: Some("gpt-5.6-luna".to_string()),
            service_tier: Some("standard".to_string()),
            tokens: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 20,
                output_tokens: 5,
                total_tokens: 105,
                ..TokenUsage::default()
            },
            request_usage_exact: true,
        }];

        let summary = summarize_calls_and_tasks(
            &tasks,
            &calls,
            SummaryWindow::new(at(1, 0), at(2, 0)).unwrap(),
            FixedOffset::east_opt(0).unwrap(),
        );

        assert_eq!(summary.projects[0].label, "repo");
        assert_eq!(summary.totals.token_usage.total_tokens, 105);
        assert_eq!(summary.totals.estimated_cost_units, 4_480);
        assert_eq!(summary.totals.api_equivalent_cost.observed_samples, 1);
        assert_eq!(summary.totals.api_equivalent_cost.priced_samples, 1);
        assert!(summary.totals.api_equivalent_cost.minimum_pico_usd.value() > 0);
    }
}
