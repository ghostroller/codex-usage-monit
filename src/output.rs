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
    let reset_credits_degraded = snapshot.rate_limit_reset_credits_partial
        || snapshot
            .rate_limit_reset_credits
            .as_ref()
            .is_some_and(|credits| {
                matches!(credits.provenance, Provenance::Stale | Provenance::Unknown)
            });
    let five_hour_partial = snapshot
        .window_analyses
        .iter()
        .find(|analysis| analysis.duration_mins == 300)
        .is_some_and(|analysis| analysis.partial);

    request.sections.iter().any(|section| match section {
        Section::Limits => limits_degraded || reset_credits_degraded,
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
            Section::Limits => {
                !snapshot.limits.is_empty() || snapshot.rate_limit_reset_credits.is_some()
            }
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
    if !request.sections.contains(&Section::Limits) {
        object.remove("rateLimitResetCredits");
        object.remove("rateLimitResetCreditsPartial");
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
    let estimate_summary_section = [
        Section::Attribution,
        Section::Models,
        Section::Tasks,
        Section::Turns,
    ]
    .into_iter()
    .find(|section| request.sections.contains(section));
    let _ = writeln!(
        output,
        "Codex usage snapshot  {}{}",
        snapshot.as_of.to_rfc3339(),
        if partial { "  [PARTIAL]" } else { "" }
    );

    if request.sections.contains(&Section::Limits) {
        let _ = writeln!(output, "\nLimits");
        if snapshot.limits.is_empty() {
            let _ = writeln!(output, "  quota windows  unavailable");
        }
        if let Some(reset_credits) = &snapshot.rate_limit_reset_credits {
            let _ = writeln!(
                output,
                "  reset credits  {} available  ({:?}, as of {})",
                reset_credits.available_count,
                reset_credits.provenance,
                reset_credits.as_of.format("%Y-%m-%d %H:%M:%S UTC")
            );
            match &reset_credits.credits {
                None => {
                    let _ = writeln!(output, "  reset credit details  unavailable");
                }
                Some(credits) => {
                    if credits.is_empty() {
                        let _ = writeln!(output, "  reset credit details  fetched, none returned");
                    }
                    for (index, credit) in credits.iter().enumerate() {
                        let label = credit
                            .title
                            .as_deref()
                            .filter(|value| !value.trim().is_empty())
                            .or_else(|| {
                                credit
                                    .description
                                    .as_deref()
                                    .filter(|value| !value.trim().is_empty())
                            })
                            .unwrap_or("untitled");
                        let reset_time = credit.expires_at.as_ref().map_or_else(
                            || "never".to_string(),
                            |expires_at| expires_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                        );
                        let _ = writeln!(
                            output,
                            "  reset credit {}  {}  status {}  type {}  granted {}  reset time {}",
                            index + 1,
                            terminal_safe_text(label),
                            terminal_safe_text(&credit.status),
                            terminal_safe_text(&credit.reset_type),
                            credit.granted_at.format("%Y-%m-%d %H:%M:%S UTC"),
                            reset_time
                        );
                    }
                    if reset_credits.details_are_truncated() {
                        let _ = writeln!(
                            output,
                            "  reset credit details  showing {}/{}",
                            credits.len(),
                            reset_credits.available_count
                        );
                    }
                    if snapshot.rate_limit_reset_credits_partial {
                        let _ = writeln!(output, "  reset credit details  partial");
                    }
                }
            }
        } else if snapshot.rate_limit_reset_credits_partial {
            let _ = writeln!(output, "  reset credits  unavailable (partial)");
        } else {
            let _ = writeln!(output, "  reset credits  unavailable");
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
        if estimate_summary_section == Some(Section::Tasks) {
            render_attribution_summary(
                &mut output,
                &snapshot.attribution,
                preferred_partial_reasons(snapshot),
            );
        }
        let _ = writeln!(
            output,
            "  {:<15} {:>12} {:>8} {:>8} {:<12}  TITLE",
            "STATUS/EVIDENCE", "TOKENS", "TOKEN5H%", "EST.Q5H", "SOURCE"
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
        if estimate_summary_section == Some(Section::Turns) {
            render_attribution_summary(
                &mut output,
                &snapshot.attribution,
                preferred_partial_reasons(snapshot),
            );
        }
        let _ = writeln!(
            output,
            "  {:<11} {:<16} {:<7} {:>12} {:>8} {:>8}  THREAD   MESSAGE",
            "STATUS", "MODEL", "EFFORT", "TOKENS", "TOKEN%", "EST.Q%"
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
        if estimate_summary_section == Some(Section::Models) {
            render_attribution_summary(
                &mut output,
                &snapshot.attribution,
                preferred_partial_reasons(snapshot),
            );
        }
        for model in &snapshot.models {
            let _ = writeln!(
                output,
                "  {:<24} {:>12}  {:>7.2}% token share  {:>8} estimated quota",
                terminal_safe_text(&model.model),
                compact_tokens(model.token_usage),
                model.local_token_share_percent,
                estimated_percent(model.estimated_quota_percent, model.quota_confidence)
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
        render_attribution_summary(
            &mut output,
            attribution,
            preferred_partial_reasons(snapshot),
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
        let _ = writeln!(output, "{}", attribution_allocation_line(attribution));
        if !analysis.partial_reasons.is_empty() {
            let _ = writeln!(
                output,
                "  partial reasons: {}",
                terminal_safe_text(&analysis.partial_reasons.join(", "))
            );
        }
        let _ = writeln!(output, "{}", attribution_quality_line(attribution));

        if attribution.local_token_usage.is_zero()
            && analysis.threads.is_empty()
            && analysis.turns.is_empty()
            && analysis.models.is_empty()
        {
            let _ = writeln!(output, "  no token events in this reset cycle");
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
                "TOKEN%", "EST.Q%", "TOKENS", "THREAD"
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
                "TOKEN%", "EST.Q%", "TOKENS", "MODEL", "EFFORT", "THREAD/TURN"
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
                "TOKEN%", "EST.Q%", "TOKENS"
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
    match confidence {
        Confidence::Unknown => "-".to_string(),
        Confidence::Low | Confidence::Medium | Confidence::High => format!("~{value:.2}%"),
    }
}

fn preferred_partial_reasons(snapshot: &Snapshot) -> &[String] {
    snapshot
        .window_analyses
        .iter()
        .find(|analysis| {
            analysis.duration_mins == 300
                && analysis
                    .attribution
                    .window
                    .as_ref()
                    .is_some_and(|window| window.limit_id.trim().eq_ignore_ascii_case("codex"))
        })
        .map(|analysis| analysis.partial_reasons.as_slice())
        .unwrap_or_default()
}

fn render_attribution_summary(
    output: &mut String,
    attribution: &crate::domain::AttributionSummary,
    partial_reasons: &[String],
) {
    let _ = writeln!(output, "{}", attribution_allocation_line(attribution));
    if !partial_reasons.is_empty() {
        let _ = writeln!(
            output,
            "  partial reasons: {}",
            terminal_safe_text(&partial_reasons.join(", "))
        );
    }
    let _ = writeln!(output, "{}", attribution_quality_line(attribution));
}

fn attribution_allocation_line(attribution: &crate::domain::AttributionSummary) -> String {
    let local_tokens = compact_tokens(attribution.local_token_usage);
    let Some(window) = attribution.window.as_ref() else {
        return format!(
            "  token total {local_tokens} | estimated - | codex quota window unavailable"
        );
    };

    if attribution.local_token_usage.total_tokens == 0 {
        return format!(
            "  token total {local_tokens} | codex gauge {:.2}% used | estimated - (no token denominator)",
            window.used_percent
        );
    }

    if attribution.confidence == Confidence::Unknown {
        return format!(
            "  token total {local_tokens} | codex gauge {:.2}% used | estimated - (estimation unavailable)",
            window.used_percent
        );
    }

    format!(
        "  token total {local_tokens} | codex gauge {:.2}% used | estimated ~{:.2}pp (gauge x short-context price share)",
        window.used_percent, attribution.proxy_projected_percent
    )
}

fn attribution_quality_line(attribution: &crate::domain::AttributionSummary) -> String {
    format!(
        "  price-weighted quota proxy, not server per-task accounting | normal Codex bucket only (Spark excluded) | external activity possible {} | settled {} | method {}",
        attribution.external_activity_possible,
        attribution.settled,
        terminal_safe_text(&attribution.method)
    )
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
mod tests;
