//! Text and JSON renderers for the shared Summary, Trends, and Health reports.
//!
//! Report builders own all data derivation. This module deliberately limits
//! itself to presentation so the CLI cannot silently use different query or
//! partial-data semantics from the TUI.

use std::fmt::Write;

use anyhow::Result;
use serde::Serialize;

use crate::api_cost::{format_api_cost_amount, format_pico_usd};
use crate::attribution::ESTIMATED_COST_UNITS_PER_CREDIT;
use crate::domain::{PicoUsd, TokenUsage, terminal_safe_text};
use crate::health_report::{HealthReport, RecorderHealthState};
use crate::output::OutputFormat;
use crate::service::ServiceState;
use crate::summary_report::{
    SummaryCoverageReport, SummaryCoverageState, SummaryMetric, SummaryReport,
    SummaryReportMetrics, SummaryTurnAttribution,
};
use crate::trends::{TrendPoint, TrendReadout, TrendReadoutValue, TrendsReport};

/// Renders a Summary report in the requested CLI output format.
pub fn render_summary_report(
    report: &SummaryReport,
    format: OutputFormat,
    compact: bool,
) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_summary_text(report)),
        OutputFormat::Json => render_summary_json(report, compact),
    }
}

/// Serializes the complete Summary wire report.
pub fn render_summary_json(report: &SummaryReport, compact: bool) -> Result<String> {
    render_json(report, compact)
}

/// Renders a complete, line-oriented Summary report.
pub fn render_summary_text(report: &SummaryReport) -> String {
    let mut output = String::new();
    let partial = summary_report_is_partial(report);
    let _ = writeln!(
        output,
        "Codex usage summary  {}{}{}",
        report.generated_at.to_rfc3339(),
        if report.api_long_context {
            "  [EST LONGX]"
        } else {
            ""
        },
        if partial { "  [PARTIAL]" } else { "" }
    );
    let _ = writeln!(
        output,
        "  range {} | grain {} | metric {}",
        report.range.label(),
        report.grain.label(),
        summary_metric_label(report.metric)
    );
    let _ = write!(
        output,
        "  window {} -> {}",
        report.window.starts_at.to_rfc3339(),
        report.window.ends_at.to_rfc3339()
    );
    if let Some(note) = report.window.note.as_deref() {
        let _ = write!(output, " ({})", terminal_safe_text(note));
    }
    output.push('\n');
    let _ = writeln!(
        output,
        "  selected value {}{}",
        format_summary_value(report.metric, report.value),
        if report.value_is_lower_bound {
            "  [LOWER BOUND]"
        } else {
            ""
        }
    );
    let _ = writeln!(
        output,
        "  totals {}",
        format_summary_metrics(report.metrics)
    );
    render_summary_coverage(&mut output, "  ", &report.coverage);
    let _ = writeln!(
        output,
        "  API chart lower bound {}",
        yes_no(report.api_chart_is_lower_bound)
    );
    if !report.partial_reasons.is_empty() {
        let _ = writeln!(
            output,
            "  partial reasons: {}",
            safe_join(&report.partial_reasons)
        );
    }

    let _ = writeln!(
        output,
        "\nChart buckets ({}, local wall clock)",
        report.buckets.len()
    );
    if report.buckets.is_empty() {
        let _ = writeln!(output, "  unavailable");
    }
    for bucket in &report.buckets {
        let _ = writeln!(
            output,
            "  {}  value {}",
            bucket.starts_at.format("%Y-%m-%d %H:%M:%S"),
            format_summary_value(report.metric, bucket.value)
        );
        let _ = writeln!(
            output,
            "    totals {}",
            format_summary_metrics(bucket.metrics)
        );
        render_summary_coverage(&mut output, "    ", &bucket.coverage);
    }

    let _ = writeln!(output, "\nProjects ({})", report.projects.len());
    if report.projects.is_empty() {
        let _ = writeln!(output, "  no represented project usage");
    }
    for project in &report.projects {
        let _ = writeln!(
            output,
            "  project {} | key={} | value {} | share {:.2}%",
            terminal_safe_text(&project.label),
            terminal_safe_text(&project.key),
            format_summary_value(report.metric, project.value),
            project.share_percent
        );
        if let Some(cwd) = project.cwd.as_deref() {
            let _ = writeln!(
                output,
                "    cwd {}",
                terminal_safe_text(&cwd.display().to_string())
            );
        }
        let _ = writeln!(
            output,
            "    totals {}",
            format_summary_metrics(project.metrics)
        );
        let _ = writeln!(
            output,
            "    chart buckets ({}, local wall clock)",
            project.buckets.len()
        );
        for bucket in &project.buckets {
            let _ = writeln!(
                output,
                "      {}  value {}  totals {}",
                bucket.starts_at.format("%Y-%m-%d %H:%M:%S"),
                format_summary_value(report.metric, bucket.value),
                format_summary_metrics(bucket.metrics)
            );
        }
        let _ = writeln!(output, "    sessions ({})", project.sessions.len());
        for session in &project.sessions {
            let _ = write!(
                output,
                "      session {} | value {} | share {:.2}%",
                terminal_safe_text(&session.thread_id),
                format_summary_value(report.metric, session.value),
                session.share_percent
            );
            if let Some(title) = session.title.as_deref() {
                let _ = write!(output, " | title={}", terminal_safe_text(title));
            }
            if let Some(source) = session.source.as_deref() {
                let _ = write!(output, " | source={}", terminal_safe_text(source));
            }
            output.push('\n');
            if let Some(cwd) = session.cwd.as_deref() {
                let _ = writeln!(
                    output,
                    "        cwd {}",
                    terminal_safe_text(&cwd.display().to_string())
                );
            }
            let _ = writeln!(
                output,
                "        totals {}",
                format_summary_metrics(session.metrics)
            );
            let _ = writeln!(output, "        turns ({})", session.turns.len());
            for turn in &session.turns {
                let _ = write!(
                    output,
                    "          turn {} | value {} | share {:.2}%",
                    summary_turn_label(turn.attribution, turn.turn_id.as_deref()),
                    format_summary_value(report.metric, turn.value),
                    turn.share_percent
                );
                if let Some(started_at) = turn.started_at {
                    let _ = write!(output, " | started={}", started_at.to_rfc3339());
                }
                if let Some(message) = turn.message_preview.as_deref() {
                    let _ = write!(output, " | message={}", terminal_safe_text(message));
                }
                output.push('\n');
                let _ = writeln!(
                    output,
                    "            totals {}",
                    format_summary_metrics(turn.metrics)
                );
            }
        }
    }

    output.trim_end().to_string()
}

/// Whether Summary coverage or its source diagnostics mark the report partial.
pub fn summary_report_is_partial(report: &SummaryReport) -> bool {
    report.coverage.state != SummaryCoverageState::Complete
        || report.coverage.source_partial
        || report.value_is_lower_bound
        || !report.partial_reasons.is_empty()
}

/// Renders a Trends report in the requested CLI output format.
pub fn render_trends_report(
    report: &TrendsReport,
    format: OutputFormat,
    compact: bool,
) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_trends_text(report)),
        OutputFormat::Json => render_trends_json(report, compact),
    }
}

/// Serializes the complete Trends wire report.
pub fn render_trends_json(report: &TrendsReport, compact: bool) -> Result<String> {
    render_json(report, compact)
}

/// Renders every Trends readout and plotted sample, including synthetic-point
/// metadata and per-point partial state.
pub fn render_trends_text(report: &TrendsReport) -> String {
    let mut output = String::new();
    let partial = trends_report_is_partial(report);
    let _ = writeln!(
        output,
        "Codex usage trends  {}{}{}",
        report.as_of.to_rfc3339(),
        if report.api_long_context_multiplier {
            "  [EST LONGX]"
        } else {
            ""
        },
        if partial { "  [PARTIAL]" } else { "" }
    );
    let _ = writeln!(output, "  day offset {}", report.day_offset);
    let _ = writeln!(
        output,
        "  15-minute window {} -> {}",
        report.half_hour_bounds[0].to_rfc3339(),
        report.half_hour_bounds[1].to_rfc3339()
    );
    let _ = writeln!(
        output,
        "  history weekly={}  15-minute={}  read-only={}  warnings={}",
        present_missing(report.weekly_history_present),
        present_missing(report.half_hour_history_present),
        yes_no(report.history_read_only),
        report.history_warning_count
    );
    for warning in &report.history_warnings {
        let _ = writeln!(output, "  warning: {}", terminal_safe_text(warning));
    }

    let _ = writeln!(output, "\nReadouts");
    render_trend_readout(
        &mut output,
        "5-hour remaining",
        report.five_hour_remaining_readout,
    );
    render_trend_readout(
        &mut output,
        "weekly remaining",
        report.weekly_remaining_readout,
    );
    render_trend_readout(&mut output, "weekly tokens", report.weekly_tokens_readout);
    render_trend_readout(
        &mut output,
        "weekly estimated",
        report.weekly_estimated_readout,
    );

    render_trend_series(&mut output, "5-hour remaining", &report.five_hour_remaining);
    render_trend_series(&mut output, "weekly remaining", &report.weekly_remaining);
    render_trend_series(&mut output, "weekly tokens", &report.weekly_tokens);
    render_trend_series(&mut output, "weekly estimated", &report.weekly_estimated);
    render_trend_series(&mut output, "15-minute tokens", &report.half_hour_tokens);
    render_trend_series(
        &mut output,
        "15-minute estimated",
        &report.half_hour_estimated,
    );

    output.trim_end().to_string()
}

/// Whether Trends has missing history, warnings, read-only history, or any
/// explicitly partial point/readout.
pub fn trends_report_is_partial(report: &TrendsReport) -> bool {
    report.history_read_only
        || report.history_warning_count > 0
        || !report.history_warnings.is_empty()
        || !report.weekly_history_present
        || !report.half_hour_history_present
        || report.five_hour_remaining.is_empty()
        || report.weekly_remaining.is_empty()
        || report.five_hour_remaining_readout.is_none()
        || report.weekly_remaining_readout.is_none()
        || report.weekly_tokens_readout.is_none()
        || report.weekly_estimated_readout.is_none()
        || [
            &report.five_hour_remaining,
            &report.weekly_remaining,
            &report.weekly_tokens,
            &report.weekly_estimated,
            &report.half_hour_tokens,
            &report.half_hour_estimated,
        ]
        .into_iter()
        .flatten()
        .any(|point| point.partial)
        || [
            report.five_hour_remaining_readout,
            report.weekly_remaining_readout,
            report.weekly_tokens_readout,
            report.weekly_estimated_readout,
        ]
        .into_iter()
        .flatten()
        .any(|readout| readout.partial)
}

/// Renders a Health report in the requested CLI output format.
pub fn render_health_report(
    report: &HealthReport,
    format: OutputFormat,
    compact: bool,
) -> Result<String> {
    match format {
        OutputFormat::Text => Ok(render_health_text(report)),
        OutputFormat::Json => render_health_json(report, compact),
    }
}

/// Serializes the complete Health wire report.
pub fn render_health_json(report: &HealthReport, compact: bool) -> Result<String> {
    render_json(report, compact)
}

/// Renders snapshot, history, recorder, and service diagnostics without
/// suppressing individual warnings or errors.
pub fn render_health_text(report: &HealthReport) -> String {
    let mut output = String::new();
    let partial = health_report_is_partial(report);
    let _ = writeln!(
        output,
        "Codex usage health  {}{}",
        report.as_of.to_rfc3339(),
        if partial { "  [PARTIAL]" } else { "" }
    );

    let snapshot = &report.snapshot;
    let _ = writeln!(
        output,
        "\nSnapshot{}",
        if snapshot.partial { "  [PARTIAL]" } else { "" }
    );
    let _ = writeln!(
        output,
        "  schema {} | as of {}",
        snapshot.schema_version,
        snapshot.as_of.to_rfc3339()
    );
    let _ = writeln!(output, "  sources ({})", snapshot.sources.len());
    for source in &snapshot.sources {
        let _ = write!(
            output,
            "    {}  {}  as-of {}",
            terminal_safe_text(&source.source),
            terminal_safe_text(&source.status),
            source.as_of.to_rfc3339()
        );
        if let Some(message) = source.message.as_deref() {
            let _ = write!(output, "  {}", terminal_safe_text(message));
        }
        output.push('\n');
    }
    let stats = &snapshot.stats;
    let _ = writeln!(
        output,
        "  files {}/{} | truncated {} | unreadable {} | lines {} | skipped {} | ambiguous resets {}",
        stats.scanned_files,
        stats.discovered_files,
        stats.truncated_files,
        stats.unreadable_files,
        stats.parsed_lines,
        stats.skipped_lines,
        stats.ambiguous_token_resets
    );
    for warning in &snapshot.warnings {
        let _ = writeln!(output, "  warning: {}", terminal_safe_text(warning));
    }
    for error in &snapshot.errors {
        let _ = writeln!(output, "  error: {}", terminal_safe_text(error));
    }

    let _ = writeln!(output, "\nHistory");
    let _ = writeln!(
        output,
        "  read-only {} | warnings {}",
        yes_no(report.history.read_only),
        report.history.warnings.len()
    );
    for warning in &report.history.warnings {
        let _ = writeln!(output, "  warning: {}", terminal_safe_text(warning));
    }

    let _ = writeln!(output, "\nRecorder");
    let _ = writeln!(
        output,
        "  state {}",
        recorder_state_label(report.recorder.state)
    );
    if let Some(status) = report.recorder.status.as_ref() {
        let namespace = status
            .history_namespace
            .as_deref()
            .map(terminal_safe_text)
            .unwrap_or_else(|| "-".to_string());
        let heartbeat = status
            .last_history_heartbeat
            .map_or_else(|| "-".to_string(), |value| value.to_rfc3339());
        let heartbeat_interval = status
            .heartbeat_interval_seconds
            .map_or_else(|| "-".to_string(), |seconds| format!("{seconds}s"));
        let _ = writeln!(
            output,
            "  schema {} | namespace {} | pid {}",
            status.schema_version, namespace, status.pid
        );
        let _ = writeln!(
            output,
            "  started {} | last attempt {} | last heartbeat {} | heartbeat interval {}",
            status.started_at.to_rfc3339(),
            status.last_attempt_at.to_rfc3339(),
            heartbeat,
            heartbeat_interval
        );
        if let Some(error) = status.last_error.as_deref() {
            let _ = writeln!(
                output,
                "  last recorder error: {}",
                terminal_safe_text(error)
            );
        }
    } else {
        let _ = writeln!(output, "  status unavailable");
    }
    if let Some(error) = report.recorder.error.as_deref() {
        let _ = writeln!(output, "  status read error: {}", terminal_safe_text(error));
    }

    let _ = writeln!(output, "\nService");
    if let Some(service) = report.service.as_ref() {
        let _ = writeln!(
            output,
            "  platform {} | state {} | installed {} | running {}",
            terminal_safe_text(&service.platform),
            service.state.label(),
            yes_no(service.installed),
            yes_no(service.running)
        );
        let registration = service.registration_path.as_deref().map_or_else(
            || "-".to_string(),
            |path| terminal_safe_text(&path.display().to_string()),
        );
        let heartbeat = service
            .last_history_heartbeat
            .map_or_else(|| "-".to_string(), |value| value.to_rfc3339());
        let _ = writeln!(output, "  registration {}", registration);
        let _ = writeln!(output, "  last history heartbeat {}", heartbeat);
        let _ = writeln!(
            output,
            "  heartbeat recent {}",
            yes_no(service.heartbeat_recent)
        );
        let _ = writeln!(output, "  detail {}", terminal_safe_text(&service.detail));
    } else {
        let _ = writeln!(output, "  unavailable");
    }
    if let Some(error) = report.service_error.as_deref() {
        let _ = writeln!(output, "  status error: {}", terminal_safe_text(error));
    }

    output.trim_end().to_string()
}

/// Whether any health component is degraded. An absent optional service and
/// an idle recorder are reported as state, but do not by themselves make a
/// one-shot health query partial.
pub fn health_report_is_partial(report: &HealthReport) -> bool {
    report.snapshot.partial
        || !report.snapshot.warnings.is_empty()
        || !report.snapshot.errors.is_empty()
        || report.history.read_only
        || !report.history.warnings.is_empty()
        || matches!(
            report.recorder.state,
            RecorderHealthState::Stale | RecorderHealthState::Error
        )
        || report.service_error.is_some()
        || report.service.as_ref().is_some_and(|service| {
            service.state == ServiceState::Unknown
                || (service.installed && (!service.running || !service.heartbeat_recent))
        })
}

fn render_json<T: Serialize>(report: &T, compact: bool) -> Result<String> {
    if compact {
        Ok(serde_json::to_string(report)?)
    } else {
        Ok(serde_json::to_string_pretty(report)?)
    }
}

fn render_summary_coverage(output: &mut String, indent: &str, coverage: &SummaryCoverageReport) {
    let _ = writeln!(
        output,
        "{indent}coverage {} {:.2}% | buckets {}/{} | represented tokens {}/{} | estimated covered tokens {} | long-context breakdown complete {} | source partial {}",
        summary_coverage_state_label(coverage.state),
        coverage.percent,
        coverage.covered_buckets,
        coverage.expected_buckets,
        format_exact_u64(coverage.represented_tokens),
        format_exact_u64(coverage.available_tokens),
        format_exact_u64(coverage.estimated_covered_tokens),
        yes_no(coverage.long_context_breakdown_complete),
        yes_no(coverage.source_partial)
    );
}

fn format_summary_metrics(metrics: SummaryReportMetrics) -> String {
    format!(
        "tokens {} | estimated base units {} | long-context extra units {} | with-long-context units {} | API equivalent {} (priced samples {}/{}, tokens {}/{}) | calls {}",
        format_token_usage(metrics.token_usage),
        format_exact_u128(metrics.estimated_cost_units),
        format_exact_u128(metrics.api_long_context_extra_cost_units),
        format_exact_u128(metrics.estimated_with_api_long_context_cost_units),
        format_api_cost_amount(metrics.api_equivalent_cost),
        metrics.api_equivalent_cost.priced_samples,
        metrics.api_equivalent_cost.observed_samples,
        format_exact_u64(metrics.api_equivalent_cost.priced_tokens),
        format_exact_u64(metrics.api_equivalent_cost.observed_tokens),
        metrics.call_count
    )
}

fn format_token_usage(usage: TokenUsage) -> String {
    format!(
        "{} (input {}, cached {}, cache-write {}, output {}, reasoning {})",
        format_exact_u64(usage.total_tokens),
        format_exact_u64(usage.input_tokens),
        format_exact_u64(usage.cached_input_tokens),
        format_exact_u64(usage.cache_write_input_tokens),
        format_exact_u64(usage.output_tokens),
        format_exact_u64(usage.reasoning_output_tokens)
    )
}

fn format_summary_value(metric: SummaryMetric, value: u128) -> String {
    match metric {
        SummaryMetric::Tokens => format!("{} tokens", format_exact_u128(value)),
        SummaryMetric::Estimated => {
            let credits = value as f64 / ESTIMATED_COST_UNITS_PER_CREDIT as f64;
            format!("{} units (~{credits:.4} credits)", format_exact_u128(value))
        }
        SummaryMetric::ApiEquivalent => format!(
            "minimum {} ({} pico-USD)",
            format_pico_usd(PicoUsd::new(value)),
            format_exact_u128(value)
        ),
    }
}

fn summary_metric_label(metric: SummaryMetric) -> &'static str {
    match metric {
        SummaryMetric::Tokens => "tokens",
        SummaryMetric::Estimated => "estimated",
        SummaryMetric::ApiEquivalent => "api-equivalent",
    }
}

fn summary_coverage_state_label(state: SummaryCoverageState) -> &'static str {
    match state {
        SummaryCoverageState::Complete => "complete",
        SummaryCoverageState::Partial => "partial",
        SummaryCoverageState::Missing => "missing",
    }
}

fn summary_turn_label(attribution: SummaryTurnAttribution, turn_id: Option<&str>) -> String {
    match attribution {
        SummaryTurnAttribution::Exact => format!(
            "exact:{}",
            terminal_safe_text(turn_id.unwrap_or("unavailable"))
        ),
        SummaryTurnAttribution::UnassignedSession => "unassigned-session".to_string(),
        SummaryTurnAttribution::UnassignedDelegated => "unassigned-delegated".to_string(),
    }
}

fn render_trend_readout(output: &mut String, label: &str, readout: Option<TrendReadout>) {
    let Some(readout) = readout else {
        let _ = writeln!(output, "  {label}: unavailable");
        return;
    };
    let _ = write!(
        output,
        "  {label}: {} | sampled {}",
        format_trend_value(readout.value),
        readout.sampled_at.to_rfc3339()
    );
    if let Some(interval) = readout.interval {
        let _ = write!(
            output,
            " | interval {} -> {}",
            interval.starts_at.to_rfc3339(),
            interval.ends_at.to_rfc3339()
        );
    }
    if readout.partial {
        let _ = write!(output, " | partial");
    }
    output.push('\n');
}

fn render_trend_series(output: &mut String, label: &str, points: &[TrendPoint]) {
    let _ = writeln!(output, "\n{label} series ({})", points.len());
    if points.is_empty() {
        let _ = writeln!(output, "  unavailable");
        return;
    }
    for point in points {
        let _ = write!(
            output,
            "  at {} | value {} | chart {:.6}",
            point.at.to_rfc3339(),
            format_trend_value(point.readout_value),
            point.value
        );
        if let Some(sampled_at) = point.sampled_at {
            let _ = write!(output, " | sampled {}", sampled_at.to_rfc3339());
        } else {
            let _ = write!(output, " | synthetic");
        }
        if let Some(interval) = point.interval {
            let _ = write!(
                output,
                " | interval {} -> {}",
                interval.starts_at.to_rfc3339(),
                interval.ends_at.to_rfc3339()
            );
        }
        if point.partial {
            let _ = write!(output, " | partial");
        }
        output.push('\n');
    }
}

fn format_trend_value(value: TrendReadoutValue) -> String {
    match value {
        TrendReadoutValue::Percent(value) if value.is_finite() => format!("{value:.2}%"),
        TrendReadoutValue::Percent(_) => "unavailable".to_string(),
        TrendReadoutValue::Tokens(value) => format!("{} tokens", format_exact_u64(value)),
    }
}

fn recorder_state_label(state: RecorderHealthState) -> &'static str {
    match state {
        RecorderHealthState::Idle => "idle",
        RecorderHealthState::Running => "running",
        RecorderHealthState::Stale => "stale",
        RecorderHealthState::Error => "error",
    }
}

fn present_missing(present: bool) -> &'static str {
    if present { "present" } else { "missing" }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn safe_join(values: &[String]) -> String {
    values
        .iter()
        .map(|value| terminal_safe_text(value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_exact_u64(value: u64) -> String {
    format_exact_digits(&value.to_string())
}

fn format_exact_u128(value: u128) -> String {
    format_exact_digits(&value.to_string())
}

fn format_exact_digits(digits: &str) -> String {
    let mut formatted = String::with_capacity(digits.len() + digits.len().saturating_sub(1) / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{ApiCostAmount, CollectionStats, SourceStatus};
    use crate::health_report::{HistoryHealth, RecorderHealth, SnapshotHealth};
    use crate::service::{RecorderStatusFile, ServiceStatus};
    use crate::summary_report::{
        SummaryBucketReport, SummaryCoverageReport, SummaryGrain, SummaryProjectBucketReport,
        SummaryProjectReport, SummaryRange, SummaryReportWindow, SummarySessionReport,
        SummaryTurnReport,
    };
    use crate::trends::{TrendInterval, TrendPoint};

    fn timestamp(hour: u32, minute: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0).unwrap()
    }

    fn metrics(tokens: u64) -> SummaryReportMetrics {
        SummaryReportMetrics {
            token_usage: TokenUsage {
                input_tokens: tokens / 2,
                output_tokens: tokens / 2,
                total_tokens: tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units: 8_000_000,
            api_long_context_extra_cost_units: 1_000_000,
            estimated_with_api_long_context_cost_units: 9_000_000,
            api_equivalent_cost: ApiCostAmount {
                minimum_pico_usd: PicoUsd::new(1_000_000_000),
                maximum_pico_usd: PicoUsd::new(1_000_000_000),
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: tokens,
                priced_tokens: tokens,
            },
            call_count: 1,
        }
    }

    fn coverage(state: SummaryCoverageState) -> SummaryCoverageReport {
        SummaryCoverageReport {
            state,
            percent: 50.0,
            expected_buckets: 2,
            covered_buckets: 1,
            available_tokens: 2_000,
            represented_tokens: 1_000,
            estimated_covered_tokens: 1_000,
            long_context_breakdown_complete: true,
            source_partial: false,
        }
    }

    fn summary_report() -> SummaryReport {
        let starts_at = timestamp(10, 0);
        let bucket_at = starts_at.naive_utc();
        let turn = SummaryTurnReport {
            attribution: SummaryTurnAttribution::Exact,
            turn_id: Some("turn-1".to_string()),
            message_preview: Some("hello\nworld".to_string()),
            started_at: Some(timestamp(10, 5)),
            metrics: metrics(1_000),
            value: 1_000,
            share_percent: 100.0,
        };
        let session = SummarySessionReport {
            thread_id: "thread-1".to_string(),
            title: Some("Example session".to_string()),
            source: Some("cli".to_string()),
            cwd: Some(PathBuf::from("/tmp/project")),
            metrics: metrics(1_000),
            value: 1_000,
            share_percent: 100.0,
            turns: vec![turn],
        };
        SummaryReport {
            schema_version: 1,
            generated_at: timestamp(12, 0),
            range: SummaryRange::SevenDays,
            grain: SummaryGrain::Hour,
            metric: SummaryMetric::Tokens,
            api_long_context: true,
            window: SummaryReportWindow {
                starts_at,
                ends_at: timestamp(12, 0),
                note: None,
            },
            metrics: metrics(1_000),
            value: 1_000,
            coverage: coverage(SummaryCoverageState::Partial),
            value_is_lower_bound: true,
            api_chart_is_lower_bound: false,
            partial_reasons: vec!["history\nwarning".to_string()],
            buckets: vec![SummaryBucketReport {
                starts_at: bucket_at,
                metrics: metrics(1_000),
                value: 1_000,
                coverage: coverage(SummaryCoverageState::Partial),
            }],
            projects: vec![SummaryProjectReport {
                key: "project-1".to_string(),
                label: "Example project".to_string(),
                cwd: Some(PathBuf::from("/tmp/project")),
                metrics: metrics(1_000),
                value: 1_000,
                share_percent: 100.0,
                buckets: vec![SummaryProjectBucketReport {
                    starts_at: bucket_at,
                    metrics: metrics(1_000),
                    value: 1_000,
                }],
                sessions: vec![session],
            }],
        }
    }

    fn trend_point(value: TrendReadoutValue, partial: bool) -> TrendPoint {
        let starts_at = timestamp(10, 0);
        TrendPoint {
            at: timestamp(10, 7),
            value: value.chart_value(),
            readout_value: value,
            sampled_at: Some(timestamp(10, 10)),
            interval: Some(TrendInterval {
                starts_at,
                ends_at: timestamp(10, 15),
            }),
            partial,
        }
    }

    fn trends_report() -> TrendsReport {
        let percent = trend_point(TrendReadoutValue::Percent(75.0), true);
        let tokens = trend_point(TrendReadoutValue::Tokens(12_345), false);
        TrendsReport {
            schema_version: 1,
            as_of: timestamp(12, 0),
            day_offset: 1,
            five_hour_remaining: vec![percent],
            weekly_remaining: vec![percent],
            weekly_tokens: vec![tokens],
            weekly_estimated: vec![percent],
            half_hour_tokens: vec![tokens],
            half_hour_estimated: vec![percent],
            five_hour_remaining_readout: percent.readout(),
            weekly_remaining_readout: percent.readout(),
            weekly_tokens_readout: tokens.readout(),
            weekly_estimated_readout: percent.readout(),
            half_hour_bounds: [timestamp(0, 0), timestamp(12, 0)],
            weekly_history_present: true,
            half_hour_history_present: true,
            history_warning_count: 1,
            history_warnings: vec!["history\nwarning".to_string()],
            history_read_only: false,
            api_long_context_multiplier: true,
        }
    }

    fn health_report() -> HealthReport {
        let now = timestamp(12, 0);
        let mut recorder = RecorderStatusFile::started(now, "namespace".to_string());
        recorder.record_success(now);
        HealthReport {
            schema_version: 1,
            as_of: now,
            snapshot: SnapshotHealth {
                schema_version: 2,
                as_of: now,
                partial: true,
                sources: vec![SourceStatus {
                    source: "rollout_jsonl".to_string(),
                    status: "partial".to_string(),
                    as_of: now,
                    message: Some("source\ndiagnostic".to_string()),
                }],
                stats: CollectionStats {
                    discovered_files: 3,
                    scanned_files: 2,
                    unreadable_files: 1,
                    ..CollectionStats::default()
                },
                warnings: vec!["snapshot warning".to_string()],
                errors: vec!["snapshot error".to_string()],
            },
            history: HistoryHealth {
                warnings: vec!["history warning".to_string()],
                read_only: true,
            },
            recorder: RecorderHealth {
                state: RecorderHealthState::Running,
                status: Some(recorder),
                error: None,
            },
            service: Some(ServiceStatus {
                platform: "linux-systemd-user".to_string(),
                state: ServiceState::Running,
                installed: true,
                running: true,
                registration_path: Some(PathBuf::from("/tmp/service")),
                last_history_heartbeat: Some(now),
                heartbeat_recent: true,
                detail: "service\ndetail".to_string(),
            }),
            service_error: Some("service\nstatus error".to_string()),
        }
    }

    #[test]
    fn summary_text_keeps_totals_coverage_tree_and_chart() {
        let text = render_summary_text(&summary_report());

        assert!(text.contains("Codex usage summary"));
        assert!(text.contains("[EST LONGX]  [PARTIAL]"));
        assert!(text.contains("selected value 1,000 tokens  [LOWER BOUND]"));
        assert!(text.contains("coverage partial 50.00% | buckets 1/2"));
        assert!(text.contains("Chart buckets (1, local wall clock)"));
        assert!(text.contains("project Example project | key=project-1"));
        assert!(text.contains("session thread-1"));
        assert!(text.contains("turn exact:turn-1"));
        assert!(text.contains("message=hello world"));
        assert!(!text.contains("hello\nworld"));
    }

    #[test]
    fn trends_text_keeps_each_series_and_readout_metadata() {
        let text = render_trends_text(&trends_report());

        assert!(text.contains("Codex usage trends"));
        assert!(text.contains("5-hour remaining: 75.00%"));
        assert!(text.contains("interval 2026-08-30T10:00:00+00:00 ->"));
        assert!(text.contains("weekly tokens series (1)"));
        assert!(text.contains("15-minute estimated series (1)"));
        assert!(text.contains("12,345 tokens"));
        assert!(text.contains("warning: history warning"));
        assert!(!text.contains("history\nwarning"));
    }

    #[test]
    fn health_text_includes_every_component_and_sanitizes_diagnostics() {
        let text = render_health_text(&health_report());

        assert!(text.contains("Snapshot  [PARTIAL]"));
        assert!(text.contains("files 2/3"));
        assert!(text.contains("History"));
        assert!(text.contains("Recorder"));
        assert!(text.contains("state running"));
        assert!(text.contains("Service"));
        assert!(text.contains("platform linux-systemd-user | state running"));
        assert!(text.contains("heartbeat recent yes"));
        assert!(text.contains("source diagnostic"));
        assert!(text.contains("detail service detail"));
        assert!(text.contains("status error: service status error"));
        assert!(!text.contains("source\ndiagnostic"));
    }

    #[test]
    fn missing_recorder_interval_does_not_gain_a_dangling_seconds_suffix() {
        let mut report = health_report();
        report
            .recorder
            .status
            .as_mut()
            .unwrap()
            .heartbeat_interval_seconds = None;

        let text = render_health_text(&report);
        assert!(text.contains("heartbeat interval -"));
        assert!(!text.contains("heartbeat interval -s"));
    }

    #[test]
    fn installed_service_requires_running_state_and_a_recent_heartbeat() {
        let mut report = health_report();
        report.snapshot.partial = false;
        report.snapshot.warnings.clear();
        report.snapshot.errors.clear();
        report.history.read_only = false;
        report.history.warnings.clear();
        report.service_error = None;
        assert!(!health_report_is_partial(&report));

        report.service.as_mut().unwrap().heartbeat_recent = false;
        assert!(health_report_is_partial(&report));
        report.service.as_mut().unwrap().heartbeat_recent = true;
        report.service.as_mut().unwrap().running = false;
        report.service.as_mut().unwrap().state = ServiceState::Stopped;
        assert!(health_report_is_partial(&report));

        let service = report.service.as_mut().unwrap();
        service.installed = false;
        service.running = false;
        service.heartbeat_recent = false;
        service.state = ServiceState::NotInstalled;
        assert!(!health_report_is_partial(&report));
    }

    #[test]
    fn missing_current_trend_readouts_are_partial() {
        let mut report = trends_report();
        report.history_read_only = false;
        report.history_warning_count = 0;
        report.history_warnings.clear();
        for point in report
            .five_hour_remaining
            .iter_mut()
            .chain(&mut report.weekly_remaining)
            .chain(&mut report.weekly_tokens)
            .chain(&mut report.weekly_estimated)
            .chain(&mut report.half_hour_tokens)
            .chain(&mut report.half_hour_estimated)
        {
            point.partial = false;
        }
        for readout in [
            &mut report.five_hour_remaining_readout,
            &mut report.weekly_remaining_readout,
            &mut report.weekly_tokens_readout,
            &mut report.weekly_estimated_readout,
        ] {
            readout.as_mut().unwrap().partial = false;
        }
        assert!(!trends_report_is_partial(&report));

        report.five_hour_remaining_readout = None;
        assert!(trends_report_is_partial(&report));
    }

    #[test]
    fn json_renderers_preserve_complete_reports_and_compactness() {
        let summary = summary_report();
        let trends = trends_report();
        let health = health_report();

        let summary_compact = render_summary_json(&summary, true).unwrap();
        let trends_pretty = render_trends_json(&trends, false).unwrap();
        let health_compact = render_health_json(&health, true).unwrap();

        assert!(!summary_compact.contains('\n'));
        assert!(trends_pretty.contains('\n'));
        assert!(!health_compact.contains('\n'));
        assert_eq!(
            serde_json::from_str::<SummaryReport>(&summary_compact).unwrap(),
            summary
        );
        assert_eq!(
            serde_json::from_str::<TrendsReport>(&trends_pretty).unwrap(),
            trends
        );
        assert_eq!(
            serde_json::from_str::<HealthReport>(&health_compact).unwrap(),
            health
        );
    }

    #[test]
    fn format_dispatch_uses_requested_mode() {
        let summary = summary_report();
        assert!(
            render_summary_report(&summary, OutputFormat::Text, true)
                .unwrap()
                .starts_with("Codex usage summary")
        );
        assert!(
            render_summary_report(&summary, OutputFormat::Json, true)
                .unwrap()
                .starts_with('{')
        );
    }
}
