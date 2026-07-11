use std::collections::BTreeSet;
use std::fmt::Write;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;

use crate::domain::{Confidence, LimitWindow, Provenance, Snapshot, TokenUsage};

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

    request.sections.iter().any(|section| match section {
        Section::Limits => limits_degraded,
        Section::Tasks | Section::Turns => rollout_degraded,
        Section::Models | Section::Attribution => rollout_degraded || limits_degraded,
        Section::Health => snapshot.partial,
    })
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
                    bucket.limit_id,
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
            "  {:<15} {:>12} {:>8} {:>8}  TITLE",
            "STATUS/EVIDENCE", "TOKENS", "LOCAL5H", "EST.Q5H"
        );
        for task in &snapshot.tasks {
            let _ = writeln!(
                output,
                "  {:<15} {:>12} {:>7.2}% {:>7.2}%  {}",
                format!(
                    "{} {}",
                    task.status.label(),
                    status_evidence(task.status_provenance, task.status_confidence)
                ),
                compact_tokens(task.token_usage),
                task.local_token_share_percent,
                task.estimated_quota_percent,
                task.title
            );
        }
    }

    if request.sections.contains(&Section::Turns) {
        let _ = writeln!(output, "\nTurns");
        let _ = writeln!(
            output,
            "  {:<10} {:<16} {:>12} {:>8} {:>8}  THREAD",
            "STATUS", "MODEL", "TOKENS", "LOCAL%", "EST.Q%"
        );
        for turn in snapshot.turns.iter().filter(|turn| {
            request
                .thread_filter
                .as_deref()
                .is_none_or(|thread| thread == turn.thread_id)
        }) {
            let _ = writeln!(
                output,
                "  {:<10} {:<16} {:>12} {:>7.2}% {:>7.2}%  {}",
                format!("{:?}", turn.status).to_lowercase(),
                turn.model.as_deref().unwrap_or("unknown"),
                compact_tokens(turn.token_usage),
                turn.local_token_share_percent,
                turn.estimated_quota_percent,
                short_id(&turn.thread_id)
            );
        }
    }

    if request.sections.contains(&Section::Models) {
        let _ = writeln!(output, "\nModels (current window)");
        for model in &snapshot.models {
            let _ = writeln!(
                output,
                "  {:<24} {:>12}  {:>7.2}% local  {:>7.2}% estimated quota  {:?}",
                model.model,
                compact_tokens(model.token_usage),
                model.local_token_share_percent,
                model.estimated_quota_percent,
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
        let _ = writeln!(
            output,
            "  local tokens {} | observed +{:.2}pp | estimated {:.2}pp | unattributed {:.2}pp",
            compact_tokens(attribution.local_token_usage),
            attribution.observed_delta_percent,
            attribution.estimated_assigned_percent,
            attribution.unattributed_percent
        );
        let _ = writeln!(
            output,
            "  confidence {:?} | coverage {:.1}% | external activity possible {} | settled {} | method {}",
            attribution.confidence,
            attribution.attribution_coverage_percent,
            attribution.external_activity_possible,
            attribution.settled,
            attribution.method
        );
    }

    if request.sections.contains(&Section::Health) {
        let _ = writeln!(output, "\nData health");
        for source in &snapshot.sources {
            let _ = writeln!(
                output,
                "  {:<16} {:<8} {}",
                source.source,
                source.status,
                source.message.as_deref().unwrap_or("")
            );
        }
        let _ = writeln!(
            output,
            "  files {}/{} | truncated {} | unreadable {} | lines {} | skipped {}",
            snapshot.stats.scanned_files,
            snapshot.stats.discovered_files,
            snapshot.stats.truncated_files,
            snapshot.stats.unreadable_files,
            snapshot.stats.parsed_lines,
            snapshot.stats.skipped_lines
        );
        for warning in &snapshot.warnings {
            let _ = writeln!(output, "  warning: {warning}");
        }
        for error in &snapshot.errors {
            let _ = writeln!(output, "  error: {error}");
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
                source.source,
                source.status,
                source.message.as_deref().unwrap_or_default()
            );
        }
        for error in &snapshot.errors {
            let _ = writeln!(output, "  error: {error}");
        }
    }

    output.trim_end().to_string()
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
        AttributionSummary, CollectionStats, LimitBucket, Provenance, SourceStatus,
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
            tasks: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            attribution: AttributionSummary::default(),
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

        assert!(!request_is_partial(&snapshot, &tasks));
        assert!(request_is_partial(&snapshot, &limits));
        let tasks_json: Value =
            serde_json::from_str(&render_output(&snapshot, &tasks).unwrap()).unwrap();
        assert_eq!(tasks_json["partial"], false);
        assert!(tasks_json.get("accountUsage").is_none());
        assert!(tasks_json.get("errors").is_none());
        let limits_json: Value =
            serde_json::from_str(&render_output(&snapshot, &limits).unwrap()).unwrap();
        assert_eq!(limits_json["partial"], true);
        assert!(limits_json.get("errors").is_some());
    }
}
