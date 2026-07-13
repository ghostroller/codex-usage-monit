use std::collections::BTreeSet;
use std::fmt::Write;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;

use crate::domain::{
    Confidence, LimitWindow, Provenance, Snapshot, TokenUsage, terminal_safe_text,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Limits,
    Tasks,
    Turns,
    Models,
    Attribution,
    Windows,
    Health,
}

impl Section {
    pub fn all() -> BTreeSet<Self> {
        BTreeSet::from([
            Self::Limits,
            Self::Tasks,
            Self::Turns,
            Self::Models,
            Self::Attribution,
            Self::Windows,
            Self::Health,
        ])
    }
}

#[derive(Clone, Debug)]
pub struct OutputRequest {
    pub format: OutputFormat,
    pub compact: bool,
    pub sections: BTreeSet<Section>,
    pub thread_filter: Option<String>,
}

pub fn render_output(snapshot: &Snapshot, request: &OutputRequest) -> Result<String> {
    match request.format {
        OutputFormat::Text => Ok(render_text(snapshot, request)),
        OutputFormat::Json => render_json(snapshot, request),
    }
}

pub fn request_is_partial(snapshot: &Snapshot, request: &OutputRequest) -> bool {
    let rollout_degraded = snapshot.sources.iter().any(|source| {
        source.source == "rollout_jsonl"
            && matches!(source.status.as_str(), "error" | "partial" | "stale")
    });
    let app_limits_degraded = snapshot.sources.iter().any(|source| {
        source.source == "app_server" && matches!(source.status.as_str(), "error" | "stale")
    });
    let limits_degraded = snapshot.limits.is_empty()
        || app_limits_degraded
        || snapshot
            .limits
            .iter()
            .any(|bucket| matches!(bucket.provenance, Provenance::Stale | Provenance::Unknown));
    let five_hour_partial = snapshot
        .window_analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 300)
        .is_some_and(|analysis| analysis.partial);

    request.sections.iter().any(|section| match section {
        Section::Limits => limits_degraded,
        Section::Tasks => {
            rollout_degraded
                || (!snapshot.tasks.is_empty() && (limits_degraded || five_hour_partial))
        }
        Section::Turns => {
            let has_matching_turn = snapshot.turns.iter().any(|turn| {
                request
                    .thread_filter
                    .as_deref()
                    .is_none_or(|thread_id| turn.thread_id == thread_id)
            });
            rollout_degraded || (has_matching_turn && (limits_degraded || five_hour_partial))
        }
        Section::Models | Section::Attribution => {
            rollout_degraded || limits_degraded || five_hour_partial
        }
        Section::Windows => {
            rollout_degraded
                || app_limits_degraded
                || snapshot
                    .window_analyses
                    .iter()
                    .any(|analysis| analysis.partial)
        }
        Section::Health => snapshot.partial,
    })
}

pub fn request_is_failure(snapshot: &Snapshot, request: &OutputRequest) -> bool {
    let rollout_complete = snapshot
        .sources
        .iter()
        .any(|source| source.source == "rollout_jsonl" && source.status == "ok");
    let mut requested_data_section = false;

    for section in &request.sections {
        let has_usable_data = match section {
            Section::Limits => !snapshot.limits.is_empty(),
            Section::Tasks => !snapshot.tasks.is_empty() || rollout_complete,
            Section::Turns => {
                snapshot.turns.iter().any(|turn| {
                    request
                        .thread_filter
                        .as_deref()
                        .is_none_or(|thread_id| turn.thread_id == thread_id)
                }) || rollout_complete
            }
            Section::Models => {
                !snapshot.models.is_empty()
                    || (snapshot.attribution.window.is_some() && rollout_complete)
            }
            Section::Attribution => {
                snapshot.attribution.window.is_some()
                    && (rollout_complete
                        || !snapshot.tasks.is_empty()
                        || !snapshot.turns.is_empty())
            }
            Section::Windows => !snapshot.window_analyses.is_empty(),
            Section::Health => continue,
        };
        requested_data_section = true;
        if has_usable_data {
            return false;
        }
    }

    requested_data_section
}

fn render_json(snapshot: &Snapshot, request: &OutputRequest) -> Result<String> {
    let mut value = serde_json::to_value(snapshot)?;
    let object = value
        .as_object_mut()
        .expect("Snapshot always serializes as an object");
    let partial = request_is_partial(snapshot, request);
    object.insert("partial".to_string(), Value::Bool(partial));

    for (section, key) in [
        (Section::Limits, "limits"),
        (Section::Tasks, "tasks"),
        (Section::Turns, "turns"),
        (Section::Models, "models"),
        (Section::Attribution, "attribution"),
        (Section::Windows, "windowAnalyses"),
    ] {
        if !request.sections.contains(&section) {
            object.remove(key);
        }
    }
    if !request.sections.contains(&Section::Health) {
        for key in ["stats", "accountUsage"] {
            object.remove(key);
        }
        if !partial {
            for key in ["sources", "warnings", "errors"] {
                object.remove(key);
            }
        }
    }

    if let Some(thread_id) = &request.thread_filter
        && let Some(Value::Array(turns)) = object.get_mut("turns")
    {
        turns.retain(|turn| {
            turn.get("threadId")
                .and_then(Value::as_str)
                .is_some_and(|value| value == thread_id)
        });
    }

    if request.compact {
        Ok(serde_json::to_string(&value)?)
    } else {
        Ok(serde_json::to_string_pretty(&value)?)
    }
}

fn render_text(snapshot: &Snapshot, request: &OutputRequest) -> String {
    let mut output = String::new();
    let partial = request_is_partial(snapshot, request);
    let _ = writeln!(
        output,
        "Codex usage snapshot  {}{}",
        snapshot.as_of.to_rfc3339(),
        if partial { "  [PARTIAL]" } else { "" }
    );

    if request.sections.contains(&Section::Limits) {
        let _ = writeln!(output, "\nLimits");
        if snapshot.limits.is_empty() {
            let _ = writeln!(output, "  unavailable");
        }
        for bucket in &snapshot.limits {
            for window in [&bucket.primary, &bucket.secondary].into_iter().flatten() {
                let _ = writeln!(
                    output,
                    "  {:<6} {:>5.1}% used  {:>5.1}% left  reset {}  ({}, {:?}, as of {})",
                    window.label(),
                    window.used_percent,
                    window.remaining_percent,
                    reset_label(window),
                    terminal_safe_text(&bucket.limit_id),
                    bucket.provenance,
                    bucket.as_of.format("%Y-%m-%d %H:%M:%S UTC")
                );
            }
        }
    }

    if request.sections.contains(&Section::Tasks) {
        let _ = writeln!(output, "\nTasks");
        let _ = writeln!(
            output,
            "  {:<15} {:>12} {:>8} {:>8} {:<12}  TITLE",
            "STATUS/EVIDENCE", "TOKENS", "LOCAL5H", "EST.Q5H", "SOURCE"
        );
        for task in &snapshot.tasks {
            let _ = writeln!(
                output,
                "  {:<15} {:>12} {:>7.2}% {:>8} {:<12}  {}",
                format!(
                    "{} {}",
                    task.status.label(),
                    status_evidence(task.status_provenance, task.status_confidence)
                ),
                compact_tokens(task.token_usage),
                task.local_token_share_percent,
                estimated_percent(task.estimated_quota_percent, task.quota_confidence),
                terminal_safe_text(task.source.as_deref().unwrap_or("unknown")),
                terminal_safe_text(&task.title)
            );
        }
    }

    if request.sections.contains(&Section::Turns) {
        let _ = writeln!(output, "\nTurns");
        let _ = writeln!(
            output,
            "  {:<11} {:<16} {:<7} {:>12} {:>8} {:>8}  THREAD   MESSAGE",
            "STATUS", "MODEL", "EFFORT", "TOKENS", "LOCAL%", "EST.Q%"
        );
        for turn in snapshot.turns.iter().filter(|turn| {
            request
                .thread_filter
                .as_deref()
                .is_none_or(|thread| thread == turn.thread_id)
        }) {
            let _ = writeln!(
                output,
                "  {:<11} {:<16} {:<7} {:>12} {:>7.2}% {:>8}  {}  {}",
                turn.status.label(),
                terminal_safe_text(turn.model.as_deref().unwrap_or("unknown")),
                terminal_safe_text(turn.reasoning_effort.as_deref().unwrap_or("unknown")),
                compact_tokens(turn.token_usage),
                turn.local_token_share_percent,
                estimated_percent(turn.estimated_quota_percent, turn.quota_confidence),
                terminal_safe_text(short_id(&turn.thread_id)),
                terminal_safe_text(turn.message_preview.as_deref().unwrap_or("-"))
            );
        }
    }

    if request.sections.contains(&Section::Models) {
        let _ = writeln!(output, "\nModels (current window)");
        for model in &snapshot.models {
            let _ = writeln!(
                output,
                "  {:<24} {:>12}  {:>7.2}% local  {:>8} estimated quota  {:?}",
                terminal_safe_text(&model.model),
                compact_tokens(model.token_usage),
                model.local_token_share_percent,
                estimated_percent(model.estimated_quota_percent, model.quota_confidence),
                model.quota_confidence
            );
        }
    }

    if request.sections.contains(&Section::Attribution) {
        let attribution = &snapshot.attribution;
        let _ = writeln!(output, "\nAttribution");
        if let Some(window) = &attribution.window {
            let _ = writeln!(
                output,
                "  {}  {} -> {}  {:.1}% used",
                window.label,
                window.starts_at.to_rfc3339(),
                window.ends_at.to_rfc3339(),
                window.used_percent
            );
        }
        let estimated_assigned = if attribution.confidence == Confidence::Unknown {
            "-".to_string()
        } else {
            format!("{:.2}pp", attribution.estimated_assigned_percent)
        };
        let _ = writeln!(
            output,
            "  local tokens {} | observed +{:.2}pp | estimated {} | unattributed {:.2}pp",
            compact_tokens(attribution.local_token_usage),
            attribution.observed_delta_percent,
            estimated_assigned,
            attribution.unattributed_percent
        );
        let _ = writeln!(
            output,
            "  confidence {:?} | coverage {:.1}% | external activity possible {} | settled {} | method {}",
            attribution.confidence,
            attribution.attribution_coverage_percent,
            attribution.external_activity_possible,
            attribution.settled,
            terminal_safe_text(&attribution.method)
        );
    }

    if request.sections.contains(&Section::Windows) {
        render_window_analyses(&mut output, snapshot);
    }

    if request.sections.contains(&Section::Health) {
        let _ = writeln!(output, "\nData health");
        for source in &snapshot.sources {
            let _ = writeln!(
                output,
                "  {:<16} {:<8} {}",
                terminal_safe_text(&source.source),
                terminal_safe_text(&source.status),
                terminal_safe_text(source.message.as_deref().unwrap_or(""))
            );
        }
        let _ = writeln!(
            output,
            "  files {}/{} | truncated {} | unreadable {} | lines {} | skipped {} | ambiguous resets {}",
            snapshot.stats.scanned_files,
            snapshot.stats.discovered_files,
            snapshot.stats.truncated_files,
            snapshot.stats.unreadable_files,
            snapshot.stats.parsed_lines,
            snapshot.stats.skipped_lines,
            snapshot.stats.ambiguous_token_resets
        );
        for warning in &snapshot.warnings {
            let _ = writeln!(output, "  warning: {}", terminal_safe_text(warning));
        }
        for error in &snapshot.errors {
            let _ = writeln!(output, "  error: {}", terminal_safe_text(error));
        }
    }

    if partial && !request.sections.contains(&Section::Health) {
        let _ = writeln!(output, "\nPartial data sources");
        for source in snapshot.sources.iter().filter(|source| {
            matches!(
                source.status.as_str(),
                "error" | "partial" | "stale" | "offline"
            )
        }) {
            let _ = writeln!(
                output,
                "  {}: {} {}",
                terminal_safe_text(&source.source),
                terminal_safe_text(&source.status),
                terminal_safe_text(source.message.as_deref().unwrap_or_default())
            );
        }
        for error in &snapshot.errors {
            let _ = writeln!(output, "  error: {}", terminal_safe_text(error));
        }
    }

    output.trim_end().to_string()
}

fn render_window_analyses(output: &mut String, snapshot: &Snapshot) {
    let _ = writeln!(output, "\nWindow analyses");
    if snapshot.window_analyses.is_empty() {
        let _ = writeln!(output, "  unavailable");
        return;
    }

    for analysis in &snapshot.window_analyses {
        let attribution = &analysis.attribution;
        if let Some(window) = &attribution.window {
            let _ = writeln!(
                output,
                "\n  {} | {} | {}m  {} -> {}  {:.1}% used{}",
                terminal_safe_text(&window.label),
                terminal_safe_text(&window.limit_id),
                analysis.duration_mins,
                window.starts_at.to_rfc3339(),
                window.ends_at.to_rfc3339(),
                window.used_percent,
                if analysis.partial { "  [PARTIAL]" } else { "" }
            );
        } else {
            let _ = writeln!(
                output,
                "\n  {}m | window descriptor unavailable",
                analysis.duration_mins
            );
        }
        let estimated_assigned = if attribution.confidence == Confidence::Unknown {
            "-".to_string()
        } else {
            format!("{:.2}pp", attribution.estimated_assigned_percent)
        };
        let _ = writeln!(
            output,
            "  local tokens {} | observed +{:.2}pp | estimated {} | unattributed {:.2}pp",
            compact_tokens(attribution.local_token_usage),
            attribution.observed_delta_percent,
            estimated_assigned,
            attribution.unattributed_percent
        );
        if !analysis.partial_reasons.is_empty() {
            let _ = writeln!(
                output,
                "  partial reasons: {}",
                terminal_safe_text(&analysis.partial_reasons.join(", "))
            );
        }
        let _ = writeln!(
            output,
            "  confidence {:?} | coverage {:.1}% | external activity possible {} | settled {} | method {}",
            attribution.confidence,
            attribution.attribution_coverage_percent,
            attribution.external_activity_possible,
            attribution.settled,
            terminal_safe_text(&attribution.method)
        );

        if attribution.local_token_usage.is_zero()
            && analysis.threads.is_empty()
            && analysis.turns.is_empty()
            && analysis.models.is_empty()
        {
            let _ = writeln!(output, "  no local token events in this reset cycle");
            continue;
        }

        let mut threads = analysis
            .threads
            .iter()
            .filter(|thread| {
                !thread.usage.token_usage.is_zero() || thread.usage.estimated_quota_percent > 0.0
            })
            .collect::<Vec<_>>();
        threads.sort_by(|left, right| {
            right
                .usage
                .token_usage
                .total_tokens
                .cmp(&left.usage.token_usage.total_tokens)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        if !threads.is_empty() {
            let _ = writeln!(output, "  Tasks");
            let _ = writeln!(
                output,
                "    {:>8} {:>8} {:>12} {:<8}  TITLE",
                "LOCAL%", "EST.Q%", "TOKENS", "THREAD"
            );
            for thread in threads {
                let title = snapshot
                    .tasks
                    .iter()
                    .find(|task| task.thread_id == thread.thread_id)
                    .map(|task| task.title.as_str())
                    .unwrap_or("-");
                let _ = writeln!(
                    output,
                    "    {:>8} {:>8} {:>12} {:<8}  {}",
                    format!("{:.2}%", thread.usage.local_token_share_percent),
                    estimated_percent(
                        thread.usage.estimated_quota_percent,
                        thread.usage.quota_confidence
                    ),
                    compact_tokens(thread.usage.token_usage),
                    terminal_safe_text(short_id(&thread.thread_id)),
                    terminal_safe_text(title)
                );
            }
        }

        let mut turns = analysis
            .turns
            .iter()
            .filter(|turn| {
                !turn.usage.token_usage.is_zero() || turn.usage.estimated_quota_percent > 0.0
            })
            .collect::<Vec<_>>();
        turns.sort_by(|left, right| {
            right
                .usage
                .token_usage
                .total_tokens
                .cmp(&left.usage.token_usage.total_tokens)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        if !turns.is_empty() {
            let _ = writeln!(output, "  Turns");
            let _ = writeln!(
                output,
                "    {:>8} {:>8} {:>12} {:<16} {:<7} {:<17}  MESSAGE",
                "LOCAL%", "EST.Q%", "TOKENS", "MODEL", "EFFORT", "THREAD/TURN"
            );
            for window_turn in turns {
                let turn = snapshot.turns.iter().find(|turn| {
                    turn.thread_id == window_turn.thread_id && turn.turn_id == window_turn.turn_id
                });
                let model = turn
                    .and_then(|turn| turn.model.as_deref())
                    .unwrap_or("unknown");
                let effort = turn
                    .and_then(|turn| turn.reasoning_effort.as_deref())
                    .unwrap_or("unknown");
                let message = turn
                    .and_then(|turn| turn.message_preview.as_deref())
                    .unwrap_or("-");
                let turn_ref = format!(
                    "{}/{}",
                    short_id(&window_turn.thread_id),
                    short_id(&window_turn.turn_id)
                );
                let _ = writeln!(
                    output,
                    "    {:>8} {:>8} {:>12} {:<16} {:<7} {:<17}  {}",
                    format!("{:.2}%", window_turn.usage.local_token_share_percent),
                    estimated_percent(
                        window_turn.usage.estimated_quota_percent,
                        window_turn.usage.quota_confidence
                    ),
                    compact_tokens(window_turn.usage.token_usage),
                    terminal_safe_text(model),
                    terminal_safe_text(effort),
                    terminal_safe_text(&turn_ref),
                    terminal_safe_text(message)
                );
            }
        }

        let mut models = analysis
            .models
            .iter()
            .filter(|model| !model.token_usage.is_zero() || model.estimated_quota_percent > 0.0)
            .collect::<Vec<_>>();
        models.sort_by(|left, right| {
            right
                .token_usage
                .total_tokens
                .cmp(&left.token_usage.total_tokens)
                .then_with(|| left.model.cmp(&right.model))
        });
        if !models.is_empty() {
            let _ = writeln!(output, "  Models");
            let _ = writeln!(
                output,
                "    {:>8} {:>8} {:>12}  MODEL",
                "LOCAL%", "EST.Q%", "TOKENS"
            );
            for model in models {
                let _ = writeln!(
                    output,
                    "    {:>8} {:>8} {:>12}  {}",
                    format!("{:.2}%", model.local_token_share_percent),
                    estimated_percent(model.estimated_quota_percent, model.quota_confidence),
                    compact_tokens(model.token_usage),
                    terminal_safe_text(&model.model)
                );
            }
        }
    }
}

fn estimated_percent(value: f64, confidence: Confidence) -> String {
    if confidence == Confidence::Unknown {
        "-".to_string()
    } else {
        format!("{value:.2}%")
    }
}

fn compact_tokens(usage: TokenUsage) -> String {
    let value = usage.total_tokens as f64;
    if value >= 1_000_000_000.0 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        usage.total_tokens.to_string()
    }
}

fn reset_label(window: &LimitWindow) -> String {
    let Some(reset) = window.resets_at else {
        return "unknown".to_string();
    };
    let remaining = reset - Utc::now();
    if remaining.num_seconds() <= 0 {
        return "due".to_string();
    }
    if remaining.num_hours() >= 24 {
        format!("{}d {}h", remaining.num_days(), remaining.num_hours() % 24)
    } else {
        format!(
            "{}h {}m",
            remaining.num_hours(),
            remaining.num_minutes() % 60
        )
    }
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn status_evidence(provenance: Provenance, confidence: Confidence) -> String {
    let provenance = match provenance {
        Provenance::Live => "LIVE",
        Provenance::ServerSnapshot => "SERVER",
        Provenance::LocalExact => "EXACT",
        Provenance::Inferred => "INFER",
        Provenance::Estimated => "EST",
        Provenance::Stale => "STALE",
        Provenance::Unknown => "UNK",
    };
    let confidence = match confidence {
        Confidence::High => "H",
        Confidence::Medium => "M",
        Confidence::Low => "L",
        Confidence::Unknown => "?",
    };
    format!("{provenance}/{confidence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AttributionSummary, CollectionStats, LimitBucket, ModelUsage, Provenance, SourceStatus,
        TaskRecord, TaskStatus, ThreadWindowUsage, TurnRecord, TurnStatus, TurnWindowUsage,
        WindowAnalysis, WindowDescriptor, WindowUsage,
    };

    #[test]
    fn compact_token_units_are_stable() {
        assert_eq!(
            compact_tokens(TokenUsage {
                total_tokens: 12_345,
                ..TokenUsage::default()
            }),
            "12.3K"
        );
    }

    #[test]
    fn token_usage_json_uses_camel_case() {
        let value = serde_json::to_value(TokenUsage {
            total_tokens: 42,
            ..TokenUsage::default()
        })
        .unwrap();
        assert_eq!(value["totalTokens"], 42);
        assert!(value.get("total_tokens").is_none());
    }

    #[test]
    fn partial_status_is_scoped_to_requested_sections() {
        let now = Utc::now();
        let window_usage = WindowUsage {
            token_usage: TokenUsage {
                total_tokens: 42,
                ..TokenUsage::default()
            },
            local_token_share_percent: 100.0,
            estimated_quota_percent: 1.25,
            quota_confidence: Confidence::Medium,
        };
        let snapshot = Snapshot {
            schema_version: 1,
            as_of: now,
            partial: true,
            codex_home: "/tmp/.codex".into(),
            sources: vec![
                SourceStatus {
                    source: "rollout_jsonl".to_string(),
                    status: "ok".to_string(),
                    as_of: now,
                    message: None,
                },
                SourceStatus {
                    source: "app_server".to_string(),
                    status: "error".to_string(),
                    as_of: now,
                    message: Some("unavailable".to_string()),
                },
            ],
            limits: vec![LimitBucket {
                limit_id: "codex".to_string(),
                limit_name: None,
                plan_type: None,
                primary: Some(LimitWindow::new(
                    10.0,
                    Some(300),
                    Some(now + chrono::Duration::hours(1)),
                )),
                secondary: None,
                credits: None,
                rate_limit_reached_type: None,
                provenance: Provenance::ServerSnapshot,
                as_of: now,
            }],
            account_usage: None,
            tasks: vec![TaskRecord {
                thread_id: "task-thread".to_string(),
                title: "task".to_string(),
                cwd: None,
                source: Some("desktop".to_string()),
                created_at: None,
                updated_at: None,
                status: TaskStatus::Completed,
                status_provenance: Provenance::LocalExact,
                status_confidence: Confidence::High,
                token_usage: TokenUsage::default(),
                turn_count: 1,
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
            }],
            turns: vec![TurnRecord {
                thread_id: "task-thread".to_string(),
                turn_id: "task-turn".to_string(),
                model: Some("gpt-test".to_string()),
                reasoning_effort: Some("xhigh".to_string()),
                message_preview: Some("message preview".to_string()),
                started_at: None,
                completed_at: None,
                duration_ms: None,
                status: TurnStatus::InProgress,
                token_usage: TokenUsage::default(),
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
            }],
            models: Vec::new(),
            attribution: AttributionSummary::default(),
            window_analyses: vec![WindowAnalysis {
                duration_mins: 10_080,
                attribution: AttributionSummary {
                    window: Some(WindowDescriptor {
                        limit_id: "codex".to_string(),
                        label: "week".to_string(),
                        starts_at: now - chrono::Duration::days(5),
                        ends_at: now + chrono::Duration::days(2),
                        used_percent: 23.0,
                    }),
                    local_token_usage: window_usage.token_usage,
                    method: "local_tokens_only".to_string(),
                    ..AttributionSummary::default()
                },
                partial: false,
                partial_reasons: Vec::new(),
                threads: vec![ThreadWindowUsage {
                    thread_id: "task-thread".to_string(),
                    usage: window_usage,
                }],
                turns: vec![TurnWindowUsage {
                    thread_id: "task-thread".to_string(),
                    turn_id: "task-turn".to_string(),
                    usage: window_usage,
                }],
                models: vec![ModelUsage {
                    model: "gpt-test".to_string(),
                    token_usage: window_usage.token_usage,
                    local_token_share_percent: 100.0,
                    estimated_quota_percent: 0.0,
                    quota_confidence: Confidence::Unknown,
                }],
            }],
            stats: CollectionStats::default(),
            warnings: Vec::new(),
            errors: vec!["app-server unavailable".to_string()],
        };
        let tasks = OutputRequest {
            format: OutputFormat::Json,
            compact: true,
            sections: BTreeSet::from([Section::Tasks]),
            thread_filter: None,
        };
        let limits = OutputRequest {
            sections: BTreeSet::from([Section::Limits]),
            ..tasks.clone()
        };

        assert!(request_is_partial(&snapshot, &tasks));
        assert!(!request_is_failure(&snapshot, &tasks));
        assert!(request_is_partial(&snapshot, &limits));
        assert!(!request_is_failure(&snapshot, &limits));
        let tasks_json: Value =
            serde_json::from_str(&render_output(&snapshot, &tasks).unwrap()).unwrap();
        assert_eq!(tasks_json["partial"], true);
        assert_eq!(tasks_json["tasks"][0]["source"], "desktop");
        assert!(tasks_json.get("windowAnalyses").is_none());
        assert!(tasks_json.get("accountUsage").is_none());
        assert!(tasks_json.get("errors").is_some());
        let turns_json: Value = serde_json::from_str(
            &render_output(
                &snapshot,
                &OutputRequest {
                    sections: BTreeSet::from([Section::Turns]),
                    ..tasks.clone()
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(turns_json["turns"][0]["messagePreview"], "message preview");
        assert_eq!(turns_json["turns"][0]["status"], "in_progress");
        let limits_json: Value =
            serde_json::from_str(&render_output(&snapshot, &limits).unwrap()).unwrap();
        assert_eq!(limits_json["partial"], true);
        assert!(limits_json.get("errors").is_some());
        let text = render_output(
            &snapshot,
            &OutputRequest {
                format: OutputFormat::Text,
                compact: false,
                sections: BTreeSet::from([Section::Tasks, Section::Turns]),
                thread_filter: None,
            },
        )
        .unwrap();
        assert!(text.contains("desktop"));
        assert!(text.contains("xhigh"));
        assert!(text.contains("in_progress"));
        assert!(text.contains("message preview"));

        let mut five_hour_partial = snapshot.clone();
        five_hour_partial.partial = false;
        five_hour_partial.errors.clear();
        for source in &mut five_hour_partial.sources {
            source.status = "ok".to_string();
            source.message = None;
        }
        five_hour_partial.window_analyses[0].duration_mins = 300;
        five_hour_partial.window_analyses[0].partial = true;
        five_hour_partial.window_analyses[0]
            .partial_reasons
            .push("multiple_active_limit_buckets".to_string());
        for section in [
            Section::Tasks,
            Section::Turns,
            Section::Models,
            Section::Attribution,
        ] {
            assert!(request_is_partial(
                &five_hour_partial,
                &OutputRequest {
                    sections: BTreeSet::from([section]),
                    ..tasks.clone()
                }
            ));
        }

        let windows = OutputRequest {
            sections: BTreeSet::from([Section::Windows]),
            ..tasks.clone()
        };
        assert!(request_is_partial(&snapshot, &windows));
        assert!(!request_is_failure(&snapshot, &windows));
        let windows_json: Value =
            serde_json::from_str(&render_output(&snapshot, &windows).unwrap()).unwrap();
        assert_eq!(windows_json["windowAnalyses"][0]["durationMins"], 10_080);
        assert_eq!(windows_json["windowAnalyses"][0]["partial"], false);
        assert!(
            windows_json["windowAnalyses"][0]
                .get("partialReasons")
                .is_none()
        );
        assert_eq!(
            windows_json["windowAnalyses"][0]["attribution"]["window"]["label"],
            "week"
        );
        assert_eq!(
            windows_json["windowAnalyses"][0]["threads"][0]["usage"]["localTokenSharePercent"],
            100.0
        );
        assert!(windows_json.get("tasks").is_none());
        assert!(windows_json.get("turns").is_none());
        assert!(windows_json.get("models").is_none());
        assert!(windows_json.get("attribution").is_none());

        let full_json: Value = serde_json::from_str(
            &render_output(
                &snapshot,
                &OutputRequest {
                    sections: Section::all(),
                    ..tasks.clone()
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert!(full_json.get("windowAnalyses").is_some());
        assert!(full_json.get("tasks").is_some());
        assert!(full_json.get("attribution").is_some());

        let windows_text = render_output(
            &snapshot,
            &OutputRequest {
                format: OutputFormat::Text,
                compact: false,
                ..windows.clone()
            },
        )
        .unwrap();
        assert!(windows_text.contains("week | codex | 10080m"));
        assert!(windows_text.contains(&(now - chrono::Duration::days(5)).to_rfc3339()));
        assert!(windows_text.contains(&(now + chrono::Duration::days(2)).to_rfc3339()));
        assert!(windows_text.contains("task"));
        assert!(windows_text.contains("gpt-test"));
        assert!(windows_text.contains("xhigh"));
        assert!(windows_text.contains("message preview"));
        assert!(windows_text.contains("100.00%"));
        assert!(windows_text.contains("        -"));

        let mut partial_window = snapshot.clone();
        partial_window.window_analyses[0].partial = true;
        partial_window.window_analyses[0]
            .partial_reasons
            .push("rollout_lookback_incomplete".to_string());
        let partial_window_text = render_output(
            &partial_window,
            &OutputRequest {
                format: OutputFormat::Text,
                compact: false,
                ..windows.clone()
            },
        )
        .unwrap();
        assert!(partial_window_text.contains("[PARTIAL]"));
        assert!(partial_window_text.contains("rollout_lookback_incomplete"));

        let mut zero_call_window = snapshot.clone();
        let analysis = &mut zero_call_window.window_analyses[0];
        analysis.attribution.local_token_usage = TokenUsage::default();
        analysis.threads.clear();
        analysis.turns.clear();
        analysis.models.clear();
        assert!(!request_is_failure(&zero_call_window, &windows));
        assert!(
            render_output(
                &zero_call_window,
                &OutputRequest {
                    format: OutputFormat::Text,
                    compact: false,
                    ..windows.clone()
                }
            )
            .unwrap()
            .contains("no local token events in this reset cycle")
        );

        let mut no_windows = snapshot.clone();
        no_windows.window_analyses.clear();
        assert!(request_is_failure(&no_windows, &windows));
        assert!(
            render_output(
                &no_windows,
                &OutputRequest {
                    format: OutputFormat::Text,
                    compact: false,
                    ..windows.clone()
                }
            )
            .unwrap()
            .contains("unavailable")
        );

        let mut empty_snapshot = snapshot.clone();
        empty_snapshot.tasks.clear();
        assert!(!request_is_partial(&empty_snapshot, &tasks));
        assert!(!request_is_failure(&empty_snapshot, &tasks));
        let empty_filtered_turns = OutputRequest {
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: Some("missing-thread".to_string()),
            ..tasks.clone()
        };
        assert!(!request_is_partial(&empty_snapshot, &empty_filtered_turns));
        assert!(!request_is_failure(&empty_snapshot, &empty_filtered_turns));

        let mut unavailable = snapshot;
        unavailable.sources[0].status = "error".to_string();
        unavailable.limits.clear();
        unavailable.tasks.clear();
        assert!(request_is_failure(&unavailable, &tasks));
        assert!(request_is_failure(&unavailable, &limits));

        unavailable.turns.push(TurnRecord {
            thread_id: "present-thread".to_string(),
            turn_id: "turn-1".to_string(),
            model: None,
            reasoning_effort: None,
            message_preview: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            status: Default::default(),
            token_usage: TokenUsage::default(),
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
        });
        let filtered_turns = OutputRequest {
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: Some("missing-thread".to_string()),
            ..tasks.clone()
        };
        assert!(request_is_failure(&unavailable, &filtered_turns));
        let matching_turns = OutputRequest {
            thread_filter: Some("present-thread".to_string()),
            ..filtered_turns
        };
        assert!(!request_is_failure(&unavailable, &matching_turns));
    }

    #[test]
    fn terminal_text_removes_control_characters() {
        assert_eq!(
            terminal_safe_text("before\u{1b}[2Jafter\u{7}"),
            "before [2Jafter "
        );
    }
}
