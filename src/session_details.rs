//! Bounded, read-only detail projection of a single local rollout.
//!
//! Conversation content stays in memory and is never added to the history cache.
//! Explicit item/turn/call identities are used instead of time-based attribution.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::api_cost::{ApiCostAccumulator, format_api_cost_amount};
use crate::bounded_io::{BoundedLine, read_bounded_line};
use crate::domain::{AgentInteraction, AgentInteractionKind, RolloutDataset, TokenUsage};

const MAX_DISCOVERY_ENTRIES: usize = 50_000;
const MAX_OWNER_PROBES: usize = 512;
const MAX_FILES: usize = 8;
const MAX_OWNER_BYTES: u64 = 64 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_READ_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECORDS: usize = 4_096;
const MAX_WARNINGS: usize = 32;
const MAX_DISPLAY_LINES: usize = 8_192;
const MAX_DISPLAY_BYTES: usize = 256 * 1024;
const DISPLAY_TRUNCATION: &str = "[display truncated: 8,192-line / 256 KiB detail display limit]";

/// Limits allocation while building the display, before the UI receives it.
/// A newline-heavy body must not first produce millions of owned strings.
#[derive(Default)]
struct DetailLines {
    lines: Vec<String>,
    bytes: usize,
    truncated: bool,
}

impl DetailLines {
    fn push(&mut self, text: String) {
        if self.truncated {
            return;
        }
        let mut pieces = text.split('\n');
        while !self.truncated {
            let Some(line) = pieces.next() else {
                break;
            };
            self.push_line(line);
        }
    }

    fn push_line(&mut self, line: &str) {
        let available = MAX_DISPLAY_BYTES.saturating_sub(DISPLAY_TRUNCATION.len() + 1 + self.bytes);
        if self.lines.len() < MAX_DISPLAY_LINES - 1 && line.len() < available {
            self.bytes += line.len() + 1;
            self.lines.push(line.to_owned());
            return;
        }
        if self.lines.len() < MAX_DISPLAY_LINES - 1 && available > 1 {
            let mut boundary = (available - 1).min(line.len());
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            if boundary > 0 {
                self.lines.push(line[..boundary].to_owned());
                self.bytes += boundary + 1;
            }
        }
        self.lines.push(DISPLAY_TRUNCATION.to_owned());
        self.bytes += DISPLAY_TRUNCATION.len() + 1;
        self.truncated = true;
    }

    fn extend(&mut self, lines: impl IntoIterator<Item = String>) {
        let mut lines = lines.into_iter();
        while !self.truncated {
            let Some(line) = lines.next() else {
                break;
            };
            self.push(line);
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn finish(self) -> Vec<String> {
        self.lines
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SessionDetails {
    pub(crate) messages: Vec<DetailMessage>,
    pub(crate) attachments: Vec<DetailEvidence>,
    pub(crate) tools: Vec<DetailTool>,
    pub(crate) metadata: BTreeMap<String, String>,
    pub(crate) usage: Vec<DetailUsage>,
    pub(crate) compactions: Vec<DetailEvidence>,
    pub(crate) failures: Vec<DetailEvidence>,
    pub(crate) file_changes: Vec<DetailEvidence>,
    pub(crate) agent_interactions: Vec<AgentInteraction>,
    pub(crate) analysis_lines: Vec<String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) files_read: usize,
    pub(crate) unassigned_records: usize,
    pub(crate) redacted: bool,
    source_discovery_partial: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailMessage {
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) timestamp: Option<DateTime<Utc>>,
    pub(crate) turn_id: Option<String>,
    pub(crate) phase: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailTool {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) timestamp: Option<DateTime<Utc>>,
    pub(crate) turn_id: Option<String>,
    pub(crate) test_command: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailUsage {
    pub(crate) timestamp: Option<DateTime<Utc>>,
    pub(crate) turn_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) tokens: TokenUsage,
    pub(crate) exact: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailEvidence {
    pub(crate) text: String,
    pub(crate) timestamp: Option<DateTime<Utc>>,
    pub(crate) turn_id: Option<String>,
}

impl SessionDetails {
    pub(crate) fn display_lines(&self) -> Vec<String> {
        let mut lines = DetailLines::default();
        lines.extend(self.analysis_lines.iter().cloned());
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("Recorded configuration and Git".to_owned());
        if self.metadata.is_empty() {
            lines.push("Configuration / recorded Git: unrecorded".to_owned());
        }
        lines.extend(
            self.metadata
                .iter()
                .map(|(key, value)| format!("{key}: {value}")),
        );
        for label in [
            "Codex version",
            "Approval policy",
            "Sandbox policy",
            "Permission profile",
            "Recorded Git branch",
            "Recorded Git commit",
            "Reported context window",
        ] {
            if !self.metadata.keys().any(|key| {
                key.contains(label)
                    || (label == "Reported context window" && key.contains("context window"))
            }) {
                lines.push(format!(
                    "{label}: {}",
                    if self.redacted {
                        "redacted"
                    } else {
                        "unrecorded"
                    }
                ));
            }
        }
        lines.push(String::new());
        append_interaction_lines(&mut lines, &self.agent_interactions);
        lines.push(format!("Recorded evidence summary: {} file change(s), {} failure/interruption record(s), {} compaction record(s), {} attachment record(s)", self.file_changes.len(), self.failures.len(), self.compactions.len(), self.attachments.len()));
        for failure in self.failures.iter().take(8) {
            let summary: String = failure
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(240)
                .collect();
            lines.push(format!(
                "Failure: {} | {} | {summary}",
                time_label(failure.timestamp),
                turn_label(failure.turn_id.as_deref())
            ));
        }
        for change in self.file_changes.iter().take(16) {
            let summary: String = change
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(240)
                .collect();
            lines.push(format!("File: {summary}"));
        }
        lines.push(String::new());
        append_evidence(
            &mut lines,
            "Recorded attachment metadata (bytes are not opened or downloaded)",
            &self.attachments,
            self.redacted,
        );
        lines.push(format!("Messages ({})", self.messages.len()));
        if self.messages.is_empty() {
            lines.push(self.absent_label("Message bodies"));
        }
        for message in &self.messages {
            if lines.truncated {
                break;
            }
            lines.push(format!(
                "{}{} | {} | {}",
                message.role,
                message
                    .phase
                    .as_ref()
                    .map(|phase| format!(" ({phase})"))
                    .unwrap_or_default(),
                time_label(message.timestamp),
                turn_label(message.turn_id.as_deref())
            ));
            append_text(&mut lines, &message.text);
            lines.push(String::new());
        }
        lines.push(format!("Tool calls ({})", self.tools.len()));
        if self.tools.is_empty() {
            lines.push(self.absent_label("Tool arguments and outputs"));
        }
        for tool in &self.tools {
            if lines.truncated {
                break;
            }
            lines.push(format!(
                "{} | {} | {}",
                tool.name,
                time_label(tool.timestamp),
                turn_label(tool.turn_id.as_deref())
            ));
            lines.push(format!("Call ID: {}", tool.call_id));
            lines.push(format!(
                "Exit: {} | Duration: {}{}",
                tool.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unrecorded".into()),
                tool.duration_ms
                    .map(|ms| format!("{ms} ms"))
                    .unwrap_or_else(|| "unrecorded".into()),
                if tool.test_command {
                    " | Command mentions a test runner; inspect result"
                } else {
                    ""
                }
            ));
            lines.push("Arguments:".into());
            append_text(
                &mut lines,
                tool.arguments.as_deref().unwrap_or("unrecorded"),
            );
            lines.push("Output:".into());
            append_text(
                &mut lines,
                tool.output
                    .as_deref()
                    .unwrap_or("unrecorded / not completed in this snapshot"),
            );
            lines.push(String::new());
        }
        append_evidence(
            &mut lines,
            "File changes and recorded diffs",
            &self.file_changes,
            self.redacted,
        );
        append_evidence(
            &mut lines,
            "Failures / interruption reasons",
            &self.failures,
            self.redacted,
        );
        append_evidence(
            &mut lines,
            "Context compactions",
            &self.compactions,
            self.redacted,
        );
        lines.push(format!("Usage observations ({})", self.usage.len()));
        lines.push("Observations below are evidence only; cumulative samples are not summed into the overview.".into());
        for usage in &self.usage {
            if lines.truncated {
                break;
            }
            lines.push(format!(
                "{} | {} | {} | tier {} | {} | total {} (in {}, cached {}, out {}, reasoning {})",
                time_label(usage.timestamp),
                turn_label(usage.turn_id.as_deref()),
                usage.model.as_deref().unwrap_or("model unrecorded"),
                usage.service_tier.as_deref().unwrap_or("unrecorded"),
                if usage.exact {
                    "native request"
                } else {
                    "reported last request; attribution incomplete"
                },
                usage.tokens.total_tokens,
                usage.tokens.input_tokens,
                usage.tokens.cached_input_tokens,
                usage.tokens.output_tokens,
                usage.tokens.reasoning_output_tokens
            ));
        }
        if self.usage.is_empty() {
            lines.push("Usage evidence: unrecorded in selected local logs".into());
        }
        lines.push(String::new());
        lines.push(format!(
            "Local source: {} rollout file(s); {} unassigned record(s)",
            self.files_read, self.unassigned_records
        ));
        lines.push("Only explicit turn / item / call identities determine turn ownership. Unassigned content is excluded from a turn detail.".into());
        lines.push(
            "Content is loaded on demand, kept in memory, and never fetched from remote sources."
                .into(),
        );
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("Note: {warning}")),
        );
        lines.finish()
    }

    fn absent_label(&self, label: &str) -> String {
        format!(
            "{label}: {}",
            if self.redacted {
                "redacted; raw rollouts were not opened"
            } else {
                "unrecorded / unavailable in selected local logs"
            }
        )
    }

    fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    fn discovery_partial(&mut self, warning: impl Into<String>) {
        self.source_discovery_partial = true;
        self.warn(warning);
    }
}

fn append_evidence(
    lines: &mut DetailLines,
    title: &str,
    evidence: &[DetailEvidence],
    redacted: bool,
) {
    lines.push(format!("{title} ({})", evidence.len()));
    if evidence.is_empty() {
        lines.push(
            if redacted {
                "redacted"
            } else {
                "unrecorded / unavailable"
            }
            .into(),
        );
    }
    for record in evidence {
        if lines.truncated {
            break;
        }
        lines.push(format!(
            "{} | {}",
            time_label(record.timestamp),
            turn_label(record.turn_id.as_deref())
        ));
        append_text(lines, &record.text);
    }
    lines.push(String::new());
}

fn append_interaction_lines(lines: &mut DetailLines, interactions: &[AgentInteraction]) {
    lines.push(format!(
        "Exact child-agent interactions ({})",
        interactions.len()
    ));
    lines.push("Child usage is not loaded by this single-thread log projection; an interaction is not a child-usage total.".into());
    if interactions.is_empty() {
        lines.push("No exact call-ID link recorded in the selected local evidence.".into());
    }
    for interaction in interactions {
        if lines.truncated {
            break;
        }
        let kind = match interaction.kind {
            AgentInteractionKind::SpawnStarted => "spawned",
            AgentInteractionKind::Interacted => "interacted",
            AgentInteractionKind::Unknown => "unknown",
        };
        lines.push(format!(
            "Turn {} {kind} child {} | call {} | {}",
            interaction.parent_turn_id,
            interaction.child_thread_id,
            interaction.call_id,
            time_label(interaction.occurred_at.or(interaction.requested_at))
        ));
    }
    lines.push(String::new());
}

fn append_text(lines: &mut DetailLines, text: &str) {
    lines.extend(text.lines().map(str::to_owned));
}

fn time_label(time: Option<DateTime<Utc>>) -> String {
    time.map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "time unrecorded".into())
}

fn turn_label(turn: Option<&str>) -> String {
    turn.map(|turn| format!("turn {turn}"))
        .unwrap_or_else(|| "unassigned".into())
}

#[cfg(test)]
pub(crate) fn load_session_details(
    codex_home: &Path,
    thread_id: &str,
    turn_id: Option<&str>,
    redact_content: bool,
) -> SessionDetails {
    load_session_details_at(
        codex_home,
        thread_id,
        turn_id,
        redact_content,
        None,
        &AtomicBool::new(false),
    )
}

#[cfg(test)]
pub(crate) fn load_session_details_at(
    codex_home: &Path,
    thread_id: &str,
    turn_id: Option<&str>,
    redact_content: bool,
    as_of: Option<DateTime<Utc>>,
    cancelled: &AtomicBool,
) -> SessionDetails {
    load_session_details_in_range(
        codex_home,
        thread_id,
        turn_id,
        redact_content,
        None,
        as_of,
        cancelled,
    )
}

pub(crate) fn load_session_details_in_range(
    codex_home: &Path,
    thread_id: &str,
    turn_id: Option<&str>,
    redact_content: bool,
    starts_at: Option<DateTime<Utc>>,
    as_of: Option<DateTime<Utc>>,
    cancelled: &AtomicBool,
) -> SessionDetails {
    let mut result = SessionDetails {
        redacted: redact_content,
        ..SessionDetails::default()
    };
    if redact_content {
        result.warn("Content redaction is enabled; original logs, parameters, paths and outputs were not read.");
        return result;
    }
    if thread_id.is_empty() || thread_id.len() > 1024 {
        result.warn("Invalid local thread identity; details were not loaded.");
        return result;
    }
    let files = discover_files(codex_home, thread_id, cancelled, &mut result);
    let mut parser = DetailParser::new(thread_id, turn_id, as_of, result);
    parser.starts_at = starts_at;
    let mut read_budget = MAX_TOTAL_READ_BYTES;
    for path in &files {
        if cancelled.load(Ordering::Relaxed) {
            parser.result.warn("Detail loading cancelled.");
            break;
        }
        let Ok(file) = crate::rollout::open_rollout_source_for_validation(path) else {
            parser
                .result
                .warn("A matching rollout could not be opened.");
            continue;
        };
        let length = file
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(MAX_FILE_BYTES);
        let limit = length.min(MAX_FILE_BYTES).min(read_budget);
        if limit == 0 {
            parser
                .result
                .warn("The 128 MiB total source-read limit was reached.");
            break;
        }
        parser.begin_file();
        parser.result.files_read += 1;
        let mut reader = BufReader::new(file.take(limit));
        let mut bytes = Vec::new();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                parser.result.warn("Detail loading cancelled.");
                break;
            }
            match read_bounded_line(&mut reader, &mut bytes, MAX_LINE_BYTES) {
                Ok(BoundedLine::Eof) => break,
                Ok(BoundedLine::TooLong) => parser
                    .result
                    .warn("An oversized rollout record was skipped (2 MiB record limit)."),
                Ok(BoundedLine::Line) => {
                    if let Ok(record) = serde_json::from_slice::<Value>(&bytes) {
                        parser.record(&record);
                    } else {
                        parser
                            .result
                            .warn("An incomplete or malformed rollout record was skipped.");
                    }
                }
                Err(_) => {
                    parser
                        .result
                        .warn("A rollout read failed; details are incomplete.");
                    break;
                }
            }
        }
        read_budget = read_budget.saturating_sub(limit);
        if length > limit {
            parser
                .result
                .warn("A rollout was truncated by the 64 MiB file / 128 MiB total read limit.");
        }
    }
    let mut result = parser.finish();
    if result.files_read == 0 {
        result.warn("No accessible local rollout was found; remote and expired history contain no message bodies.");
    }
    if !cancelled.load(Ordering::Relaxed) {
        let cutoff = as_of.unwrap_or_else(Utc::now);
        let dataset =
            crate::rollout::load_detail_usage(&files, codex_home, thread_id, cutoff, cancelled);
        add_usage_analysis(&mut result, dataset, thread_id, turn_id, starts_at, cutoff);
        add_current_git(&mut result, cancelled);
    }
    result
}

fn discover_files(
    root: &Path,
    thread: &str,
    cancelled: &AtomicBool,
    result: &mut SessionDetails,
) -> Vec<PathBuf> {
    let mut named = Vec::new();
    let mut other = Vec::new();
    let mut entries = 0;
    for directory in ["sessions", "archived_sessions"] {
        let directory = root.join(directory);
        if !directory.exists() {
            continue;
        }
        for entry in WalkDir::new(directory).follow_links(false).max_depth(12) {
            if cancelled.load(Ordering::Relaxed) {
                return Vec::new();
            }
            entries += 1;
            if entries > MAX_DISCOVERY_ENTRIES {
                result.discovery_partial("Rollout discovery reached its 50,000-entry limit; older files may be unavailable.");
                break;
            }
            let Ok(entry) = entry else {
                result.discovery_partial(
                    "A rollout directory entry was inaccessible; local discovery is incomplete.",
                );
                continue;
            };
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|ext| ext.to_str()) != Some("jsonl")
            {
                continue;
            }
            if entry.file_name().to_string_lossy().contains(thread) {
                named.push(entry.into_path());
            } else if other.len() < MAX_OWNER_PROBES {
                other.push(entry.into_path());
            } else {
                result.discovery_partial("The 512-file fallback discovery limit was reached; additional copied rollouts may be unavailable.");
            }
        }
    }
    named.sort();
    other.sort();
    let mut files = Vec::new();
    let mut probes = 0;
    for path in named.into_iter().chain(other) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        probes += 1;
        if probes > MAX_OWNER_PROBES {
            result.discovery_partial("The 512-file ownership-probe limit was reached; other local copies may be unavailable.");
            break;
        }
        let owner = probe_owner(&path);
        if owner.is_none() {
            result.discovery_partial("A rollout owner could not be established within the header probe; its local details are unavailable.");
        }
        if owner.as_deref() == Some(thread) {
            if files.len() == MAX_FILES {
                result.discovery_partial(
                    "The 8-rollout detail limit was reached; extra copies were excluded.",
                );
                break;
            }
            files.push(path);
        }
    }
    files
}

fn probe_owner(path: &Path) -> Option<String> {
    let file = crate::rollout::open_rollout_source_for_validation(path).ok()?;
    let mut reader = BufReader::new(file.take(MAX_OWNER_BYTES));
    let mut bytes = Vec::new();
    for _ in 0..32 {
        match read_bounded_line(&mut reader, &mut bytes, MAX_OWNER_BYTES as usize).ok()? {
            BoundedLine::Eof => return None,
            BoundedLine::TooLong => continue,
            BoundedLine::Line => {
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                if string(&value, &["type"]) == Some("session_meta") {
                    return string(value.get("payload")?, &["id"]).map(str::to_owned);
                }
            }
        }
    }
    None
}

struct DetailParser<'a> {
    thread: &'a str,
    selected_turn: Option<&'a str>,
    as_of: Option<DateTime<Utc>>,
    starts_at: Option<DateTime<Utc>>,
    result: SessionDetails,
    owner_seen: bool,
    owner_created_at: Option<DateTime<Utc>>,
    foreign: bool,
    content_bytes: usize,
    record_count: usize,
    seen: HashSet<String>,
    tool_index: HashMap<String, usize>,
    call_turns: HashMap<String, String>,
    conflicted_calls: HashSet<String>,
    item_turns: HashMap<String, String>,
    turn_settings: HashMap<String, (Option<String>, Option<String>)>,
    native_usage_seen: HashSet<String>,
    previous_usage: Option<TokenUsage>,
}

impl<'a> DetailParser<'a> {
    fn new(
        thread: &'a str,
        selected_turn: Option<&'a str>,
        as_of: Option<DateTime<Utc>>,
        result: SessionDetails,
    ) -> Self {
        Self {
            thread,
            selected_turn,
            as_of,
            starts_at: None,
            result,
            owner_seen: false,
            owner_created_at: None,
            foreign: false,
            content_bytes: 0,
            record_count: 0,
            seen: HashSet::new(),
            tool_index: HashMap::new(),
            call_turns: HashMap::new(),
            conflicted_calls: HashSet::new(),
            item_turns: HashMap::new(),
            turn_settings: HashMap::new(),
            native_usage_seen: HashSet::new(),
            previous_usage: None,
        }
    }

    fn begin_file(&mut self) {
        self.owner_seen = false;
        self.owner_created_at = None;
        self.foreign = false;
        self.previous_usage = None;
    }

    fn record(&mut self, record: &Value) {
        let Some(payload) = record.get("payload") else {
            return;
        };
        let record_type = string(record, &["type"]);
        let time = record.get("timestamp").and_then(timestamp);
        if let Some(as_of) = self.as_of {
            match time {
                Some(time) if time > as_of => return,
                None => {
                    self.result
                        .warn("Records without timestamps were excluded from the frozen snapshot.");
                    return;
                }
                _ => {}
            }
        }
        if record_type == Some("session_meta") {
            let owner = string(payload, &["id"]);
            if !self.owner_seen {
                if owner != Some(self.thread) {
                    self.foreign = true;
                    return;
                }
                self.owner_seen = true;
                self.owner_created_at = payload.get("timestamp").and_then(timestamp).or(time);
                self.foreign = is_inherited(payload);
                self.metadata(payload, None);
            } else {
                self.foreign = owner != Some(self.thread);
            }
            return;
        }
        if !self.owner_seen {
            return;
        }
        let explicit_thread = string(payload, &["thread_id", "threadId"]);
        if explicit_thread.is_some_and(|thread| thread != self.thread) {
            return;
        }
        if self.foreign {
            if explicit_thread == Some(self.thread)
                || crate::rollout::starts_owning_segment(
                    record_type,
                    payload.as_object(),
                    self.thread,
                    self.owner_created_at,
                )
            {
                self.foreign = false;
            } else {
                return;
            }
        }
        let kind = string(payload, &["type"]).unwrap_or("");
        let turn = string(payload, &["turn_id", "turnId"])
            .or_else(|| {
                payload
                    .get("internal_chat_message_metadata_passthrough")
                    .or_else(|| payload.get("internalChatMessageMetadataPassthrough"))
                    .and_then(|metadata| string(metadata, &["turn_id", "turnId"]))
            })
            .map(str::to_owned);
        if record_type == Some("turn_context") {
            if self.accept_turn(turn.as_deref()) {
                self.metadata(payload, turn.as_deref());
            }
            if let Some(turn) = turn
                && self.turn_settings.len() < MAX_RECORDS
            {
                self.turn_settings.insert(
                    turn,
                    (
                        string(payload, &["model"]).map(str::to_owned),
                        string(payload, &["service_tier", "serviceTier"]).map(str::to_owned),
                    ),
                );
            }
            return;
        }
        if self
            .starts_at
            .is_some_and(|starts_at| time.is_none_or(|time| time < starts_at))
        {
            return;
        }
        if kind == "item_completed" {
            if let Some(item) = payload.get("item") {
                let item_id = string(item, &["id", "call_id", "callId"]);
                if let (Some(id), Some(turn)) = (item_id, turn.as_ref())
                    && self.item_turns.len() < MAX_RECORDS
                {
                    self.item_turns.insert(id.to_owned(), turn.clone());
                }
                self.completed_item(item, turn, time);
            }
            return;
        }
        if record_type == Some("token_usage_record") {
            self.native_usage(payload, turn, time);
            return;
        }
        if kind == "token_count" {
            self.usage_sample(payload, turn.clone(), time);
            if self
                .selected_turn
                .is_none_or(|selected| turn.as_deref() == Some(selected))
                && let Some(info) = payload.get("info")
            {
                self.context_window(info, turn.as_deref());
            }
            return;
        }
        if kind == "task_started" && self.accept_turn(turn.as_deref()) {
            self.context_window(payload, turn.as_deref());
        }
        if kind == "thread_settings_applied"
            && let (Some(turn), Some(settings)) = (
                turn.as_ref(),
                payload
                    .get("thread_settings")
                    .or_else(|| payload.get("threadSettings")),
            )
            && self.turn_settings.len() < MAX_RECORDS
        {
            self.turn_settings.insert(
                turn.clone(),
                (
                    string(settings, &["model"]).map(str::to_owned),
                    string(settings, &["service_tier", "serviceTier"]).map(str::to_owned),
                ),
            );
        }
        if record_type == Some("response_item") {
            match kind {
                "message" => self.message(payload, turn, time),
                "function_call" | "custom_tool_call" => self.tool_call(payload, turn, time),
                "function_call_output" | "custom_tool_call_output" => {
                    self.tool_output(payload, turn, time)
                }
                _ => {}
            }
        } else if record_type == Some("event_msg") {
            match kind {
                "user_message" | "agent_message" => self.event_message(payload, turn, time),
                "task_complete" => {
                    if let Some(text) = string(payload, &["last_agent_message"]) {
                        self.push_message(
                            "assistant",
                            text.to_owned(),
                            turn,
                            time,
                            Some("final_answer".into()),
                            None,
                        );
                    }
                }
                "turn_aborted" | "task_failed" | "error" | "turn_failed" => {
                    let text = string(payload, &["message", "reason", "error"])
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{kind}: no reason recorded"));
                    if self.accept_turn(turn.as_deref())
                        && let Some(text) = self.keep_text(text)
                    {
                        self.result.failures.push(DetailEvidence {
                            text,
                            timestamp: time,
                            turn_id: turn,
                        });
                    }
                }
                _ => {}
            }
        } else if record_type == Some("compacted") && self.accept_turn(turn.as_deref()) {
            let number = payload
                .get("window_number")
                .and_then(Value::as_u64)
                .map(|number| format!("; window {number}"))
                .unwrap_or_default();
            if let Some(text) = self.keep_text(format!("Context compaction recorded{number}; replacement history is excluded from messages.")) { self.result.compactions.push(DetailEvidence { text, timestamp: time, turn_id: turn }); }
        }
    }

    fn metadata(&mut self, payload: &Value, turn: Option<&str>) {
        let prefix = turn.map(|turn| format!("Turn {turn} ")).unwrap_or_default();
        for (key, label) in [
            ("cli_version", "Codex version"),
            ("cwd", "Recorded working directory"),
            ("model", "Model"),
            ("effort", "Reasoning effort"),
            ("approval_policy", "Approval policy"),
            ("sandbox_policy", "Sandbox policy"),
            ("permission_profile", "Permission profile"),
            ("model_provider", "Model provider"),
        ] {
            if let Some(value) = payload.get(key) {
                let value = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                if let Some(value) = self.keep_text(value) {
                    self.result
                        .metadata
                        .insert(format!("{prefix}{label}"), value);
                }
            }
        }
        if let Some(git) = payload.get("git") {
            for (key, label) in [
                ("branch", "Recorded Git branch"),
                ("commit_hash", "Recorded Git commit"),
                ("repository_url", "Recorded Git repository"),
            ] {
                if let Some(value) = string(git, &[key])
                    && let Some(value) = self.keep_text(value.to_owned())
                {
                    self.result.metadata.insert(label.to_owned(), value);
                }
            }
        }
        self.context_window(payload, turn);
    }

    fn context_window(&mut self, payload: &Value, turn: Option<&str>) {
        if let Some(size) = payload.get("model_context_window").and_then(Value::as_u64)
            && self.result.metadata.len() < MAX_RECORDS
        {
            let label = turn
                .map(|turn| format!("Turn {turn} context window"))
                .unwrap_or_else(|| "Unassigned reported context window".into());
            self.result
                .metadata
                .insert(label, format!("{size} tokens (capacity; not occupancy)"));
        }
    }

    fn accept_turn(&mut self, turn: Option<&str>) -> bool {
        if turn.is_none() {
            self.result.unassigned_records = self.result.unassigned_records.saturating_add(1);
        }
        self.selected_turn
            .is_none_or(|selected| turn == Some(selected))
    }

    fn keep_text(&mut self, text: String) -> Option<String> {
        if self.record_count >= MAX_RECORDS || self.content_bytes >= MAX_CONTENT_BYTES {
            self.result
                .warn("Details reached the 4,096-record / 2 MiB retained-content limit.");
            return None;
        }
        self.record_count += 1;
        let limit = MAX_TEXT_BYTES.min(MAX_CONTENT_BYTES - self.content_bytes);
        let mut text = text;
        if text.len() > limit {
            let marker = "\n[truncated: detail text limit]";
            let mut boundary = limit.saturating_sub(marker.len());
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text.truncate(boundary);
            if limit >= marker.len() {
                text.push_str(marker);
            }
            self.result.warn("A message, argument, output or diff was truncated at the 64 KiB per-field / 2 MiB total-content limit.");
        }
        self.content_bytes = self.content_bytes.saturating_add(text.len());
        Some(text)
    }

    fn push_message(
        &mut self,
        role: &str,
        text: String,
        turn: Option<String>,
        time: Option<DateTime<Utc>>,
        phase: Option<String>,
        id: Option<&str>,
    ) {
        if !self.accept_turn(turn.as_deref()) || text.is_empty() {
            return;
        }
        let identity = id.map(|id| format!("message-id:{id}")).unwrap_or_else(|| {
            format!(
                "message:{}:{}:{:?}:{:?}:{:x}",
                role,
                turn.as_deref().unwrap_or(""),
                time,
                phase,
                Sha256::digest(text.as_bytes())
            )
        });
        if self.seen.len() >= MAX_RECORDS || !self.seen.insert(identity) {
            return;
        }
        if let Some(text) = self.keep_text(text) {
            self.result.messages.push(DetailMessage {
                role: role.into(),
                text,
                timestamp: time,
                turn_id: turn,
                phase,
            });
        }
    }

    fn message(&mut self, payload: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let role = string(payload, &["role"]).unwrap_or("unknown");
        if !matches!(role, "user" | "assistant") {
            return;
        }
        let id = string(payload, &["id"]);
        let turn = turn.or_else(|| id.and_then(|id| self.item_turns.get(id).cloned()));
        self.collect_attachments(payload, turn.as_deref(), time);
        if let Some(text) = content_text(payload) {
            self.push_message(
                role,
                text,
                turn,
                time,
                string(payload, &["phase"]).map(str::to_owned),
                id,
            );
        }
    }

    fn event_message(
        &mut self,
        payload: &Value,
        turn: Option<String>,
        time: Option<DateTime<Utc>>,
    ) {
        let role = if string(payload, &["type"]) == Some("user_message") {
            "user"
        } else {
            "assistant"
        };
        self.collect_attachments(payload, turn.as_deref(), time);
        if let Some(text) = string(payload, &["message"]) {
            self.push_message(role, text.to_owned(), turn, time, None, None);
        }
    }

    fn completed_item(&mut self, item: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let kind = string(item, &["type"]).unwrap_or("");
        match kind {
            "UserMessage" | "user_message" | "AgentMessage" | "agent_message" => {
                let role = if matches!(kind, "UserMessage" | "user_message") {
                    "user"
                } else {
                    "assistant"
                };
                self.collect_attachments(item, turn.as_deref(), time);
                if let Some(text) = content_text(item) {
                    self.push_message(
                        role,
                        text,
                        turn,
                        time,
                        string(item, &["phase"]).map(str::to_owned),
                        string(item, &["id"]),
                    );
                }
            }
            "CommandExecution" | "command_execution" | "McpToolCall" | "mcp_tool_call"
            | "DynamicToolCall" | "dynamic_tool_call" | "ToolCall" => {
                self.completed_tool(item, turn, time);
            }
            "FileChange" | "file_change" => {
                if !self.accept_turn(turn.as_deref()) {
                    return;
                }
                let id = string(item, &["id"]);
                if self.seen.len() >= MAX_RECORDS
                    || id.is_some_and(|id| !self.seen.insert(format!("file:{id}")))
                {
                    return;
                }
                if let Some(changes) = item.get("changes").and_then(Value::as_object) {
                    for (path, change) in changes {
                        let status = string(item, &["status"]).unwrap_or("status unrecorded");
                        let kind = string(change, &["type"]).unwrap_or("change type unrecorded");
                        let diff =
                            string(change, &["unified_diff", "diff"]).unwrap_or("diff unrecorded");
                        let moved = string(change, &["move_path"])
                            .map(|path| format!(" -> {path}"))
                            .unwrap_or_default();
                        if let Some(text) =
                            self.keep_text(format!("{path}{moved} | {kind} | {status}\n{diff}"))
                        {
                            self.result.file_changes.push(DetailEvidence {
                                text,
                                timestamp: time,
                                turn_id: turn.clone(),
                            });
                        }
                    }
                }
            }
            "ContextCompaction" | "context_compaction" => {
                if self.accept_turn(turn.as_deref()) && let Some(text) = self.keep_text("Context compaction recorded; occupancy and compression ratio are unrecorded.".into()) { self.result.compactions.push(DetailEvidence { text, timestamp: time, turn_id: turn }); }
            }
            _ => {}
        }
    }

    fn tool_call(&mut self, payload: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let Some(call_id) = string(payload, &["call_id", "callId", "id"]) else {
            return;
        };
        if !self.register_call_turn(call_id, turn.as_deref()) {
            return;
        }
        let turn = turn.or_else(|| self.call_turns.get(call_id).cloned());
        // Keep unassigned calls until exact completion evidence may resolve them.
        if let Some(existing) = self.tool_index.get(call_id).copied() {
            if self.result.tools[existing].turn_id.is_none() {
                self.result.tools[existing].turn_id = turn;
            }
            return;
        }
        let name = string(payload, &["name", "tool"]).unwrap_or("unnamed tool");
        let arguments = payload
            .get("arguments")
            .or_else(|| payload.get("input"))
            .filter(|value| !value.is_null())
            .map(value_text);
        let test_command =
            arguments.as_deref().is_some_and(is_test_command) && is_command_tool(name);
        let arguments = arguments.and_then(|text| self.keep_text(text));
        if self.result.tools.len() >= MAX_RECORDS {
            self.result.warn("The tool-record limit was reached.");
            return;
        }
        self.tool_index
            .insert(call_id.to_owned(), self.result.tools.len());
        self.result.tools.push(DetailTool {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
            output: None,
            exit_code: None,
            duration_ms: None,
            timestamp: time,
            turn_id: turn,
            test_command,
        });
    }

    fn tool_output(&mut self, payload: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let Some(call_id) = string(payload, &["call_id", "callId", "id"]) else {
            return;
        };
        if !self.register_call_turn(call_id, turn.as_deref()) {
            return;
        }
        let output = payload
            .get("output")
            .filter(|value| !value.is_null())
            .map(value_text);
        let Some(index) = self.tool_index.get(call_id).copied() else {
            if !self.accept_turn(turn.as_deref()) {
                return;
            }
            self.result
                .warn("A tool output had no matching call ID; its arguments/name are unrecorded.");
            self.tool_call(
                &serde_json::json!({"call_id":call_id,"name":"unmatched tool output"}),
                turn.clone(),
                time,
            );
            if self.tool_index.contains_key(call_id) {
                self.tool_output(payload, turn, time);
            }
            return;
        };
        let output = output.and_then(|text| self.keep_text(text));
        let exit = integer(payload, &["exit_code", "exitCode"])
            .or_else(|| output.as_deref().and_then(output_exit));
        let duration = unsigned(payload, &["duration_ms", "durationMs"])
            .or_else(|| output.as_deref().and_then(output_duration));
        let tool = &mut self.result.tools[index];
        if tool.turn_id.is_none() {
            tool.turn_id = turn.or_else(|| self.call_turns.get(call_id).cloned());
        }
        if output.is_some() {
            tool.output = output;
        }
        if exit.is_some() {
            tool.exit_code = exit;
        }
        if duration.is_some() {
            tool.duration_ms = duration;
        }
    }

    fn completed_tool(&mut self, item: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let Some(id) = string(item, &["call_id", "callId", "id"]) else {
            return;
        };
        if !self.register_call_turn(id, turn.as_deref()) {
            return;
        }
        let name = string(item, &["name", "tool", "command"]).unwrap_or("recorded tool execution");
        let arguments = item.get("arguments").or_else(|| item.get("command"));
        self.tool_call(
            &serde_json::json!({"call_id":id,"name":name,"arguments":arguments}),
            turn.clone(),
            time,
        );
        let output = item
            .get("aggregated_output")
            .or_else(|| item.get("output"))
            .or_else(|| item.get("result"))
            .or_else(|| item.get("stdout"));
        self.tool_output(&serde_json::json!({"call_id":id,"output":output,"exit_code":integer(item,&["exit_code","exitCode"]),"duration_ms":unsigned(item,&["duration_ms","durationMs"])}), turn.clone(), time);
        if let Some(index) = self.tool_index.get(id).copied() {
            let tool = &mut self.result.tools[index];
            tool.turn_id = turn;
            tool.test_command = tool.arguments.as_deref().is_some_and(is_test_command);
        }
    }

    fn native_usage(&mut self, payload: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        if string(payload, &["thread_id", "threadId"]) != Some(self.thread)
            || !self.accept_turn(turn.as_deref())
        {
            return;
        }
        let Some(response) = string(payload, &["response_id", "responseId"]) else {
            return;
        };
        if self.native_usage_seen.len() >= MAX_RECORDS
            || !self.native_usage_seen.insert(response.to_owned())
        {
            return;
        }
        let Some(tokens) = payload.get("usage").and_then(parse_usage) else {
            self.result
                .warn("Invalid native request token breakdown was excluded.");
            return;
        };
        let (model, tier) = turn
            .as_ref()
            .and_then(|turn| self.turn_settings.get(turn))
            .cloned()
            .unwrap_or_default();
        self.result.usage.push(DetailUsage {
            timestamp: time,
            turn_id: turn,
            model,
            service_tier: tier,
            tokens,
            exact: true,
        });
    }

    fn register_call_turn(&mut self, call_id: &str, turn: Option<&str>) -> bool {
        if self.conflicted_calls.contains(call_id) {
            return false;
        }
        let Some(turn) = turn else {
            return true;
        };
        if self
            .call_turns
            .get(call_id)
            .is_some_and(|previous| previous != turn)
        {
            if self.conflicted_calls.len() < MAX_RECORDS {
                self.conflicted_calls.insert(call_id.to_owned());
            }
            self.call_turns.remove(call_id);
            if let Some(index) = self.tool_index.get(call_id).copied() {
                let tool = &mut self.result.tools[index];
                tool.turn_id = None;
                tool.arguments = None;
                tool.output = None;
                tool.exit_code = None;
                tool.duration_ms = None;
                tool.name = "conflicting call identity (content withheld)".into();
            }
            self.result.warn("The same tool call ID carried conflicting explicit turn IDs; its content was withheld and it remains unassigned.");
            return false;
        }
        if self.call_turns.len() < MAX_RECORDS {
            self.call_turns.insert(call_id.to_owned(), turn.to_owned());
        }
        true
    }

    fn collect_attachments(
        &mut self,
        payload: &Value,
        turn: Option<&str>,
        time: Option<DateTime<Utc>>,
    ) {
        if self.result.redacted
            || !self
                .selected_turn
                .is_none_or(|selected| turn == Some(selected))
        {
            return;
        }
        let mut candidates = Vec::new();
        for key in ["content", "attachments"] {
            if let Some(parts) = payload.get(key).and_then(Value::as_array) {
                for part in parts {
                    if let Some(kind) =
                        string(part, &["type"]).filter(|kind| is_attachment_kind(kind))
                    {
                        candidates.push((kind, part));
                    } else {
                        for kind in ["input_image", "image", "input_file", "file", "attachment"] {
                            if let Some(value) = part.get(kind) {
                                candidates.push((kind, value));
                            }
                        }
                    }
                }
            }
        }
        for kind in ["input_image", "image", "input_file", "file", "attachment"] {
            if let Some(value) = payload.get(kind) {
                candidates.push((kind, value));
            }
        }
        let item_id = string(payload, &["id"]);
        for (index, (kind, value)) in candidates.into_iter().enumerate() {
            let text = attachment_metadata(kind, value);
            let identity = format!(
                "attachment:{}:{}:{:?}:{index}:{:x}",
                item_id.unwrap_or(""),
                turn.unwrap_or(""),
                time,
                Sha256::digest(text.as_bytes())
            );
            if self.seen.len() >= MAX_RECORDS || !self.seen.insert(identity) {
                continue;
            }
            if let Some(text) = self.keep_text(text) {
                self.result.attachments.push(DetailEvidence {
                    text,
                    timestamp: time,
                    turn_id: turn.map(str::to_owned),
                });
            }
        }
    }

    fn usage_sample(&mut self, payload: &Value, turn: Option<String>, time: Option<DateTime<Utc>>) {
        let Some(info) = payload.get("info") else {
            return;
        };
        let Some(total) = info.get("total_token_usage").and_then(parse_usage) else {
            self.previous_usage = None;
            self.result
                .warn("Invalid cumulative token breakdown was excluded.");
            return;
        };
        if self.previous_usage == Some(total) {
            return;
        }
        let previous = self.previous_usage.replace(total);
        let Some(last) = info.get("last_token_usage").and_then(parse_usage) else {
            return;
        };
        // This is explicitly labelled sample evidence, never a new overview sum.
        // A reset/unknown gap must not make the entire cumulative counter a request.
        if previous.is_some_and(|previous| total.delta_from(previous).is_none()) && last != total {
            self.result.warn(
                "A cumulative counter reset was observed; the ambiguous sample was excluded.",
            );
            return;
        }
        if last.total_tokens > total.total_tokens || total.delta_from(last).is_none() {
            self.result
                .warn("Inconsistent last-request tokens were excluded.");
            return;
        }
        if !self.accept_turn(turn.as_deref()) {
            return;
        }
        let identity = format!("usage:{:?}:{:?}:{:?}", time, total, last);
        if self.seen.len() >= MAX_RECORDS
            || !self.seen.insert(identity)
            || self.result.usage.len() >= MAX_RECORDS
        {
            return;
        }
        let (model, tier) = turn
            .as_ref()
            .and_then(|turn| self.turn_settings.get(turn))
            .cloned()
            .unwrap_or_default();
        self.result.usage.push(DetailUsage {
            timestamp: time,
            turn_id: turn,
            model,
            service_tier: tier,
            tokens: last,
            exact: false,
        });
    }

    fn finish(mut self) -> SessionDetails {
        for tool in &mut self.result.tools {
            if self.conflicted_calls.contains(&tool.call_id) {
                tool.turn_id = None;
            } else if tool.turn_id.is_none() {
                tool.turn_id = self.call_turns.get(&tool.call_id).cloned();
            }
        }
        self.result.unassigned_records += self
            .result
            .tools
            .iter()
            .filter(|tool| tool.turn_id.is_none())
            .count();
        if let Some(selected) = self.selected_turn {
            self.result
                .tools
                .retain(|tool| tool.turn_id.as_deref() == Some(selected));
        }
        self.result
            .messages
            .sort_by_key(|message| message.timestamp);
        self.result.tools.sort_by_key(|tool| tool.timestamp);
        self.result.usage.sort_by_key(|usage| usage.timestamp);
        self.result
    }
}

fn string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
}
fn integer(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_i64))
}
fn unsigned(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}
fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn content_text(payload: &Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|part| string(part, &["text"]))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn is_attachment_kind(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "input_image" | "image" | "input_file" | "file" | "attachment"
    )
}

fn attachment_metadata(kind: &str, value: &Value) -> String {
    let mut lines = vec![format!("Attachment type: {kind}")];
    collect_attachment_fields(value, "", 0, &mut lines);
    if lines.len() == 1 {
        lines.push("Metadata: unrecorded".into());
    }
    lines.join("\n")
}

fn collect_attachment_fields(value: &Value, prefix: &str, depth: usize, lines: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    if let Some(text) = value.as_str() {
        if embedded_data_url(text) {
            lines.push(format!(
                "{}: embedded attachment present; data URL bytes omitted",
                if prefix.is_empty() { "value" } else { prefix }
            ));
        } else if !text.is_empty() {
            lines.push(format!(
                "{}: {}",
                if prefix.is_empty() {
                    "recorded value"
                } else {
                    prefix
                },
                attachment_field_text(text)
            ));
        }
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for key in [
        "type",
        "url",
        "image_url",
        "file_url",
        "file_id",
        "fileId",
        "path",
        "file_path",
        "filename",
        "file_name",
        "mime_type",
        "mime",
        "media_type",
        "name",
        "title",
        "detail",
    ] {
        let Some(field) = object.get(key) else {
            continue;
        };
        if key == "type" && prefix.is_empty() {
            continue;
        }
        let label = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some(text) = field.as_str().filter(|text| !text.is_empty()) {
            if embedded_data_url(text) {
                lines.push(format!(
                    "{label}: embedded attachment present; data URL bytes omitted"
                ));
            } else {
                lines.push(format!("{label}: {}", attachment_field_text(text)));
            }
        } else if field.is_object() {
            collect_attachment_fields(field, &label, depth + 1, lines);
        }
    }
    for key in [
        "input_image",
        "image",
        "input_file",
        "file",
        "attachment",
        "source",
    ] {
        if let Some(nested) = object.get(key) {
            let label = if prefix.is_empty() {
                key.to_owned()
            } else {
                format!("{prefix}.{key}")
            };
            collect_attachment_fields(nested, &label, depth + 1, lines);
        }
    }
    for key in ["data", "file_data", "image_data", "base64", "bytes"] {
        if object.get(key).is_some_and(|value| !value.is_null()) {
            lines.push(format!(
                "{prefix}{key}: embedded attachment present; bytes omitted"
            ));
        }
    }
}

fn attachment_field_text(text: &str) -> String {
    const LIMIT: usize = 4096;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let mut boundary = LIMIT;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!(
        "{} [truncated: attachment metadata field limit]",
        &text[..boundary]
    )
}

fn embedded_data_url(text: &str) -> bool {
    text.trim_start()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn is_inherited(payload: &Value) -> bool {
    string(
        payload,
        &[
            "parent_thread_id",
            "parentThreadId",
            "forked_from_id",
            "forkedFromId",
        ],
    )
    .is_some()
        || payload.get("source").is_some_and(|source| {
            source.get("subagent").is_some() || source.get("subAgent").is_some()
        })
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    for key in [
        "input_tokens",
        "inputTokens",
        "cached_input_tokens",
        "cachedInputTokens",
        "cache_write_input_tokens",
        "cacheWriteInputTokens",
        "output_tokens",
        "outputTokens",
        "reasoning_output_tokens",
        "reasoningOutputTokens",
        "unclassified_tokens",
        "unclassifiedTokens",
        "total_tokens",
        "totalTokens",
    ] {
        if value.get(key).is_some_and(|value| value.as_u64().is_none()) {
            return None;
        }
    }
    let tokens = TokenUsage {
        input_tokens: unsigned(value, &["input_tokens", "inputTokens"]).unwrap_or(0),
        cached_input_tokens: unsigned(value, &["cached_input_tokens", "cachedInputTokens"])
            .unwrap_or(0),
        cache_write_input_tokens: unsigned(
            value,
            &["cache_write_input_tokens", "cacheWriteInputTokens"],
        )
        .unwrap_or(0),
        output_tokens: unsigned(value, &["output_tokens", "outputTokens"]).unwrap_or(0),
        reasoning_output_tokens: unsigned(
            value,
            &["reasoning_output_tokens", "reasoningOutputTokens"],
        )
        .unwrap_or(0),
        unclassified_tokens: unsigned(value, &["unclassified_tokens", "unclassifiedTokens"])
            .unwrap_or(0),
        total_tokens: unsigned(value, &["total_tokens", "totalTokens"])?,
    };
    tokens.has_valid_breakdown().then_some(tokens)
}

fn output_exit(output: &str) -> Option<i64> {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        return integer(&value, &["exit_code", "exitCode"]);
    }
    if !output.starts_with("Chunk ID:") && !output.starts_with("Wall time:") {
        return None;
    }
    output
        .lines()
        .take_while(|line| *line != "Output:")
        .take(8)
        .find_map(|line| {
            line.strip_prefix("Process exited with code ")
                .and_then(|code| code.trim().parse().ok())
        })
}
fn output_duration(output: &str) -> Option<u64> {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        return unsigned(&value, &["duration_ms", "durationMs"]);
    }
    if !output.starts_with("Chunk ID:") && !output.starts_with("Wall time:") {
        return None;
    }
    output
        .lines()
        .take_while(|line| *line != "Output:")
        .take(8)
        .find_map(|line| {
            let seconds = line
                .strip_prefix("Wall time: ")?
                .strip_suffix(" seconds")?
                .trim()
                .parse::<f64>()
                .ok()?;
            (seconds.is_finite() && seconds >= 0.0 && seconds <= u64::MAX as f64 / 1000.0)
                .then_some((seconds * 1000.0) as u64)
        })
}
fn is_command_tool(name: &str) -> bool {
    name.ends_with("exec_command")
        || name.ends_with("shell")
        || name.ends_with("shell_command")
        || name.ends_with("run_terminal_cmd")
}
fn is_test_command(text: &str) -> bool {
    [
        "cargo test",
        "cargo nextest",
        "pytest",
        "python -m unittest",
        "python3 -m unittest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "go test",
        "dotnet test",
        "verify-unix.sh",
        "test-linux-docker.sh",
        "test-windows-utm.py",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

fn add_usage_analysis(
    result: &mut SessionDetails,
    dataset: RolloutDataset,
    thread_id: &str,
    turn_id: Option<&str>,
    starts_at: Option<DateTime<Utc>>,
    as_of: DateTime<Utc>,
) {
    let source_partial = result.source_discovery_partial
        || !dataset.warnings.is_empty()
        || dataset.stats.skipped_lines > 0
        || dataset.stats.unreadable_files > 0
        || dataset.stats.truncated_files > 0;
    for warning in dataset.warnings {
        result.warn(warning);
    }
    let in_range =
        |time: DateTime<Utc>| time <= as_of && starts_at.is_none_or(|starts_at| time >= starts_at);
    let mut calls: Vec<_> = dataset
        .calls
        .into_iter()
        .filter(|call| {
            call.thread_id == thread_id
                && turn_id.is_none_or(|turn_id| call.turn_id.as_deref() == Some(turn_id))
                && in_range(call.timestamp)
        })
        .collect();
    calls.sort_by_key(|call| call.timestamp);
    let mut models: BTreeMap<String, (TokenUsage, ApiCostAccumulator)> = BTreeMap::new();
    let mut hours: BTreeMap<String, (TokenUsage, ApiCostAccumulator)> = BTreeMap::new();
    let mut total = TokenUsage::default();
    let mut cost = ApiCostAccumulator::default();
    let mut previous_model: Option<&str> = None;
    let mut switches = 0usize;
    let mut unassigned = 0usize;
    for call in &calls {
        total.add_assign(call.tokens);
        cost.add_call(call);
        let model = call.model.as_deref().unwrap_or("unrecorded");
        if call.model.is_some() {
            if previous_model.is_some_and(|previous| previous != model) {
                switches += 1;
            }
            previous_model = Some(model);
        } else {
            previous_model = None;
        }
        if call.turn_id.is_none() {
            unassigned += 1;
        }
        if models.len() < MAX_RECORDS || models.contains_key(model) {
            let entry = models.entry(model.to_owned()).or_default();
            entry.0.add_assign(call.tokens);
            entry.1.add_call(call);
        } else {
            result.warn(
                "Model distribution exceeded its 4,096-group limit; additional models are omitted.",
            );
        }
        let hour = call.timestamp.format("%Y-%m-%d %H:00 UTC").to_string();
        if hours.len() < MAX_RECORDS || hours.contains_key(&hour) {
            let entry = hours.entry(hour).or_default();
            entry.0.add_assign(call.tokens);
            entry.1.add_call(call);
        } else {
            result
                .warn("Hourly trend exceeded its 4,096-hour limit; additional hours are omitted.");
        }
    }
    let exact_requests = calls.iter().filter(|call| call.request_usage_exact).count();
    let amount = cost.amount();
    let mut lines = vec!["Collected model and hourly usage (bounded local evidence)".into()];
    lines.push("These totals cover the discovered log subset in the selected range. Copies, native request records and cumulative counters use the existing collector's deduplication/reset rules; no inferred aliases or tiers.".into());
    lines.push(format!(
        "Observed tokens: {} | {} usage samples ({} exact request samples) | {} unassigned samples",
        if calls.is_empty() {
            "unrecorded".into()
        } else {
            total.total_tokens.to_string()
        },
        calls.len(),
        exact_requests,
        unassigned
    ));
    let amount_label = if calls.is_empty() {
        "unrecorded".into()
    } else {
        format_api_cost_amount(amount)
    };
    lines.push(format!(
        "API equivalent: {amount_label}{} | priced tokens {}/{} | observed known-model switches: {switches}",
        if source_partial { " (partial source coverage)" } else { "" },
        amount.priced_tokens,
        amount.observed_tokens
    ));
    let partial_reasons = cost.summary().partial_reasons;
    if !partial_reasons.is_empty() {
        lines.push(format!("Pricing notes: {}", partial_reasons.join(", ")));
    }
    if let Some(latest) = calls.last() {
        lines.push(format!(
            "Last observed usage: {}",
            latest.timestamp.to_rfc3339()
        ));
    }
    if let Some(request) = calls.iter().rev().find(|call| call.request_usage_exact) {
        let capacity_key = request
            .turn_id
            .as_ref()
            .map(|turn| format!("Turn {turn} context window"));
        let capacity = capacity_key
            .as_ref()
            .and_then(|key| result.metadata.get(key))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|capacity| *capacity > 0);
        if let Some(capacity) = capacity {
            lines.push(format!("Latest recorded exact-request input / recorded turn capacity: {} / {capacity} tokens ({:.1}%) at {}; proxy for that request, not live context occupancy", request.tokens.input_tokens, request.tokens.input_tokens as f64 * 100.0 / capacity as f64, request.timestamp.to_rfc3339()));
        } else {
            lines.push(
                "Request-input / context-capacity proxy: unrecorded (no matched turn capacity)."
                    .into(),
            );
        }
    } else {
        lines.push(
            "Request-input / context-capacity proxy: unrecorded (no exact request sample).".into(),
        );
    }
    lines.push("Model distribution:".into());
    if models.is_empty() {
        lines.push("unrecorded / no usage evidence in range".into());
    }
    for (model, (tokens, accumulator)) in models {
        let share = if total.total_tokens == 0 {
            0.0
        } else {
            tokens.total_tokens as f64 * 100.0 / total.total_tokens as f64
        };
        let amount = accumulator.amount();
        lines.push(format!(
            "{model}: {} tokens ({share:.1}%) | API {} | priced {}/{} tokens",
            tokens.total_tokens,
            format_api_cost_amount(amount),
            amount.priced_tokens,
            amount.observed_tokens
        ));
    }
    lines.push("Hourly token / API trend (UTC):".into());
    if hours.is_empty() {
        lines.push("unrecorded / no usage evidence in range".into());
    }
    for (hour, (tokens, accumulator)) in hours {
        let amount = accumulator.amount();
        lines.push(format!(
            "{hour}: {} tokens | API {} | priced {}/{} tokens",
            tokens.total_tokens,
            format_api_cost_amount(amount),
            amount.priced_tokens,
            amount.observed_tokens
        ));
    }
    result.analysis_lines = lines;
    result.agent_interactions = dataset
        .agent_interactions
        .into_iter()
        .filter(|interaction| {
            interaction.parent_thread_id == thread_id
                && turn_id.is_none_or(|turn| interaction.parent_turn_id == turn)
                && interaction
                    .occurred_at
                    .or(interaction.requested_at)
                    .is_some_and(in_range)
        })
        .take(MAX_RECORDS)
        .collect();
}

fn add_current_git(result: &mut SessionDetails, cancelled: &AtomicBool) {
    let cwd = result
        .metadata
        .get("Recorded working directory")
        .cloned()
        .or_else(|| {
            result
                .metadata
                .iter()
                .find(|(key, _)| key.ends_with("Recorded working directory"))
                .map(|(_, value)| value.clone())
        });
    let Some(cwd) = cwd
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute() && cwd.is_dir())
    else {
        return;
    };
    let mut succeeded = false;
    for (label, args) in [
        (
            "Current Git branch",
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
        ),
        ("Current Git commit", vec!["rev-parse", "--verify", "HEAD"]),
        (
            "Current Git tracked-file status",
            vec!["status", "--porcelain=v1", "--untracked-files=no"],
        ),
    ] {
        let mut command = Command::new("git");
        command
            .args([
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "-C",
            ])
            .arg(&cwd)
            .args(args);
        match crate::bounded_process::output_cancellable(
            &mut command,
            Duration::from_millis(500),
            16 * 1024,
            || cancelled.load(Ordering::Relaxed),
        ) {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                result.metadata.insert(
                    label.into(),
                    if text.is_empty() {
                        "clean (tracked files only)".into()
                    } else {
                        text
                    },
                );
                succeeded = true;
            }
            _ => {
                result.warn("Current Git state was unavailable within the read-only probe bounds.");
                break;
            }
        }
    }
    if succeeded {
        result.metadata.insert(
            "Current Git checked at".into(),
            format!(
                "{}; current workspace state, not a turn result",
                Utc::now().to_rfc3339()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(kind: &str, payload: Value) -> Value {
        json!({"type":kind,"timestamp":"2026-10-04T01:00:00Z","payload":payload})
    }
    fn parser<'a>(turn: Option<&'a str>) -> DetailParser<'a> {
        let mut parser = DetailParser::new("thread", turn, None, SessionDetails::default());
        parser.record(&record("session_meta", json!({"id":"thread"})));
        parser
    }
    fn usage(input: u64, cached: u64, output: u64) -> Value {
        json!({"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"total_tokens":input+output})
    }

    #[cfg(unix)]
    #[test]
    fn session_details_owner_probe_rejects_links_and_nonregular_sources() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let text = record("session_meta", json!({"id":"thread"})).to_string() + "\n";
        let regular = root.path().join("regular.jsonl");
        std::fs::write(&regular, &text).unwrap();
        assert_eq!(probe_owner(&regular).as_deref(), Some("thread"));

        let link = root.path().join("link.jsonl");
        symlink(&regular, &link).unwrap();
        assert!(probe_owner(&link).is_none());

        let fifo = root.path().join("pipe.jsonl");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // Publish a complete owner record without a startup wait. An unsafe
        // reader could accept it, so this also verifies descriptor type checks.
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
        assert!(probe_owner(&fifo).is_none());
    }

    #[test]
    fn session_details_exact_turn_filter_excludes_unassigned_and_other_turns() {
        let mut parser = parser(Some("one"));
        for (turn, text) in [
            (Some("one"), "selected"),
            (Some("two"), "other"),
            (None, "ambiguous"),
        ] {
            parser.record(&record(
                "event_msg",
                json!({"type":"user_message","turn_id":turn,"message":text}),
            ));
        }
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, "selected");
        assert_eq!(result.unassigned_records, 1);
    }

    #[test]
    fn session_details_fork_embedded_parent_history_is_excluded() {
        let mut parser = DetailParser::new("child", None, None, SessionDetails::default());
        parser.record(&record(
            "session_meta",
            json!({"id":"child","parent_thread_id":"parent"}),
        ));
        parser.record(&record(
            "response_item",
            json!({"type":"message","role":"user","content":[{"text":"inherited parent body"}]}),
        ));
        parser.record(&record("session_meta", json!({"id":"parent"})));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","message":"another inherited body","turn_id":"old"}),
        ));
        parser.record(&record("event_msg", json!({"type":"item_completed","thread_id":"child","turn_id":"new","item":{"type":"UserMessage","id":"own","content":[{"text":"own body"}]}})));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, "own body");
    }

    #[test]
    fn session_details_redaction_never_opens_original_rollouts() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("thread.jsonl"), "secret invalid JSON").unwrap();
        let result = load_session_details(root.path(), "thread", None, true);
        assert!(result.redacted);
        assert_eq!(result.files_read, 0);
        assert!(result.messages.is_empty());
        assert!(!result.display_lines().join("\n").contains("secret"));
    }

    #[test]
    fn session_details_tool_output_joins_only_matching_call_identity() {
        let mut parser = parser(Some("one"));
        parser.record(&record("response_item", json!({"type":"function_call","name":"exec_command","arguments":"cargo test sample","call_id":"call"})));
        parser.record(&record("response_item", json!({"type":"function_call_output","call_id":"call","output":"Wall time: 0.12 seconds\nProcess exited with code 7\nOutput:\nfailed"})));
        parser.record(&record("event_msg", json!({"type":"item_completed","thread_id":"thread","turn_id":"one","item":{"type":"CommandExecution","id":"call","command":"cargo test sample","aggregated_output":"failed","exit_code":7,"duration_ms":120}})));
        parser.record(&record("response_item", json!({"type":"function_call","name":"exec_command","call_id":"unassigned","arguments":"do not borrow turn one"})));
        let result = parser.finish();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].turn_id.as_deref(), Some("one"));
        assert_eq!(result.tools[0].exit_code, Some(7));
        assert_eq!(result.tools[0].duration_ms, Some(120));
        assert!(result.tools[0].test_command);
    }

    #[test]
    fn session_details_text_limit_preserves_utf8_and_reports_truncation() {
        let mut parser = parser(None);
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"界".repeat(MAX_TEXT_BYTES)}),
        ));
        let result = parser.finish();
        assert!(result.messages[0].text.len() < MAX_TEXT_BYTES + 100);
        assert!(result.messages[0].text.contains("truncated"));
        assert!(!result.warnings.is_empty());
    }

    #[test]
    fn session_details_tokens_validate_cached_subsets_and_do_not_repeat_cumulative() {
        let mut parser = parser(None);
        let sample = record(
            "event_msg",
            json!({"type":"token_count","info":{"total_token_usage":usage(100,30,20),"last_token_usage":usage(100,30,20)}}),
        );
        parser.record(&sample);
        parser.record(&sample);
        parser.record(&record("event_msg", json!({"type":"token_count","info":{"total_token_usage":usage(100,101,20),"last_token_usage":usage(100,101,20)}})));
        let result = parser.finish();
        assert_eq!(result.usage.len(), 1);
        assert_eq!(result.usage[0].tokens.total_tokens, 120);
        assert_eq!(result.usage[0].tokens.cached_input_tokens, 30);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("Invalid cumulative"))
        );
    }

    #[test]
    fn session_details_frozen_snapshot_excludes_future_and_undated_records() {
        let cutoff = DateTime::parse_from_rfc3339("2026-10-04T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut parser = DetailParser::new("thread", None, Some(cutoff), SessionDetails::default());
        parser.record(&record("session_meta", json!({"id":"thread"})));
        parser.record(&json!({"type":"event_msg","timestamp":"2026-10-04T03:00:00Z","payload":{"type":"user_message","message":"future"}}));
        parser.record(
            &json!({"type":"event_msg","payload":{"type":"user_message","message":"undated"}}),
        );
        assert!(parser.finish().messages.is_empty());
    }

    #[test]
    fn session_details_copy_owner_and_file_diff_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let records = [
            record("session_meta", json!({"id":"thread"})),
            record(
                "event_msg",
                json!({"type":"item_completed","thread_id":"thread","turn_id":"one","item":{"type":"FileChange","id":"change","status":"Completed","changes":{"src/a.rs":{"type":"Update","unified_diff":"-before\n+after"}}}}),
            ),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            root.path().join("sessions/rollout-copied_otherid.jsonl"),
            text,
        )
        .unwrap();
        let result = load_session_details(root.path(), "thread", Some("one"), false);
        assert_eq!(result.files_read, 1);
        assert_eq!(result.file_changes.len(), 1);
        assert!(result.file_changes[0].text.contains("+after"));
    }

    #[test]
    fn session_details_output_metadata_does_not_read_exit_codes_from_command_stdout() {
        assert_eq!(
            output_exit("a test printed\nProcess exited with code 0"),
            None
        );
        assert_eq!(
            output_exit("Wall time: 0.1 seconds\nOutput:\nProcess exited with code 0"),
            None
        );
        assert_eq!(
            output_exit(
                "Wall time: 0.1 seconds\nProcess exited with code 7\nOutput:\nProcess exited with code 0"
            ),
            Some(7)
        );
        assert_eq!(output_duration("stdout\nWall time: 99 seconds"), None);
        assert!(parse_usage(&json!({"input_tokens":-1,"total_tokens":0})).is_none());
    }

    #[test]
    fn session_details_range_keeps_configuration_but_excludes_earlier_bodies() {
        let mut parser = parser(None);
        parser.starts_at = Some(
            DateTime::parse_from_rfc3339("2026-10-04T02:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        parser.record(&record(
            "turn_context",
            json!({"turn_id":"one","model":"gpt-5.6-sol","approval_policy":"on-request"}),
        ));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"before range"}),
        ));
        let result = parser.finish();
        assert!(result.messages.is_empty());
        assert!(result.metadata.values().any(|value| value == "on-request"));
    }

    #[test]
    fn session_details_safe_collector_reconciles_native_mirror_copies_and_future() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let records = [
            record("session_meta", json!({"id":"thread"})),
            record(
                "event_msg",
                json!({"type":"task_started","turn_id":"one","model_context_window":1000}),
            ),
            record(
                "turn_context",
                json!({"turn_id":"one","model":"gpt-5.6-sol"}),
            ),
            record(
                "token_usage_record",
                json!({"thread_id":"thread","turn_id":"one","response_id":"request","usage":usage(100,30,20),"turn_token_usage":usage(100,30,20)}),
            ),
            record(
                "event_msg",
                json!({"type":"token_count","info":{"total_token_usage":usage(100,30,20),"last_token_usage":usage(100,30,20)}}),
            ),
            json!({"type":"token_usage_record","timestamp":"2026-10-04T03:00:00Z","payload":{"thread_id":"thread","turn_id":"one","response_id":"future","usage":usage(1000,0,200)}}),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(root.path().join("sessions/rollout-thread.jsonl"), &text).unwrap();
        std::fs::write(
            root.path().join("sessions/rollout-thread_copy.jsonl"),
            &text,
        )
        .unwrap();
        let cutoff = DateTime::parse_from_rfc3339("2026-10-04T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let result = load_session_details_at(
            root.path(),
            "thread",
            Some("one"),
            false,
            Some(cutoff),
            &AtomicBool::new(false),
        );
        let display = result.analysis_lines.join("\n");
        assert!(
            display.contains("Observed tokens: 120 | 1 usage samples (1 exact request samples)"),
            "{display}"
        );
        assert!(display.contains("100 / 1000 tokens (10.0%)"), "{display}");
        assert!(!display.contains("1200 tokens"));
    }

    #[test]
    fn session_details_missing_usage_is_unrecorded_not_zero_cost() {
        let mut result = SessionDetails::default();
        add_usage_analysis(
            &mut result,
            RolloutDataset::default(),
            "thread",
            None,
            None,
            Utc::now(),
        );
        assert!(
            result
                .analysis_lines
                .iter()
                .any(|line| line.contains("API equivalent: unrecorded"))
        );
    }

    #[test]
    fn session_details_subagent_own_turn_without_thread_id_resumes_after_parent_history() {
        let child = "019a0ee0-0000-7000-8000-000000000000";
        let parent_turn = "019a0edf-ffff-7000-8000-000000000000";
        let own_turn = "019a0ee0-0001-7000-8000-000000000000";
        let mut parser = DetailParser::new(child, Some(own_turn), None, SessionDetails::default());
        parser.record(&record(
            "session_meta",
            json!({"id":child,"source":{"subagent":{"thread_spawn":{}}}}),
        ));
        parser.record(&record(
            "event_msg",
            json!({"type":"task_started","turn_id":parent_turn}),
        ));
        parser.record(&record("event_msg", json!({"type":"item_completed","turn_id":parent_turn,"item":{"type":"UserMessage","id":"inherited","content":[{"text":"parent body"}]}})));
        parser.record(&record(
            "event_msg",
            json!({"type":"task_started","turn_id":own_turn,"model_context_window":1000}),
        ));
        parser.record(&record(
            "turn_context",
            json!({"turn_id":own_turn,"model":"gpt-5.6-sol","approval_policy":"on-request"}),
        ));
        parser.record(&record("event_msg", json!({"type":"item_completed","turn_id":own_turn,"item":{"type":"UserMessage","id":"own","content":[{"text":"own body"}]}})));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, "own body");
        assert_eq!(result.messages[0].turn_id.as_deref(), Some(own_turn));
        assert!(result.metadata.values().any(|value| value == "on-request"));
        assert!(
            result
                .metadata
                .contains_key(&format!("Turn {own_turn} context window"))
        );
    }

    #[test]
    fn session_details_fork_own_started_at_resumes_without_uuid_or_thread_id() {
        let mut parser = DetailParser::new("child", None, None, SessionDetails::default());
        parser.record(&record(
            "session_meta",
            json!({"id":"child","parent_thread_id":"parent"}),
        ));
        parser.record(&record("event_msg", json!({"type":"task_started","turn_id":"parent-turn","started_at":"2026-10-04T00:50:00Z"})));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"parent-turn","message":"parent"}),
        ));
        parser.record(&record("event_msg", json!({"type":"task_started","turn_id":"child-turn","started_at":"2026-10-04T01:00:01Z"})));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"child-turn","message":"child"}),
        ));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, "child");
    }

    #[test]
    fn session_details_selected_turn_context_window_excludes_other_and_unassigned_samples() {
        let mut parser = parser(Some("one"));
        for (turn, capacity) in [(Some("one"), 1000), (Some("two"), 2000), (None, 3000)] {
            parser.record(&record("event_msg", json!({"type":"token_count","turn_id":turn,"info":{"model_context_window":capacity,"total_token_usage":usage(100,0,20),"last_token_usage":usage(100,0,20)}})));
        }
        let result = parser.finish();
        assert_eq!(
            result
                .metadata
                .get("Turn one context window")
                .map(String::as_str),
            Some("1000 tokens (capacity; not occupancy)")
        );
        assert!(
            !result
                .metadata
                .keys()
                .any(|key| key.contains("two") || key.contains("Unassigned"))
        );
    }

    #[test]
    fn session_details_discovery_exclusion_marks_usage_source_partial() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        for number in 0..=MAX_FILES {
            let text = [record("session_meta", json!({"id":"thread"})), record("token_usage_record", json!({"thread_id":"thread","turn_id":"one","response_id":format!("request-{number}"),"usage":usage(10,0,2)}))].iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
            std::fs::write(
                sessions.join(format!("rollout-thread-{number}.jsonl")),
                text,
            )
            .unwrap();
        }
        let result = load_session_details(root.path(), "thread", None, false);
        assert_eq!(result.files_read, MAX_FILES);
        assert!(result.source_discovery_partial);
        assert!(result.analysis_lines.iter().any(
            |line| line.contains("API equivalent:") && line.contains("partial source coverage")
        ));
        assert!(
            result
                .warnings
                .iter()
                .any(|line| line.contains("extra copies were excluded"))
        );
    }

    #[test]
    fn session_details_conflicting_tool_turns_are_withheld_for_every_update_path() {
        for update in [
            record(
                "response_item",
                json!({"type":"function_call","name":"exec_command","arguments":"two arguments","call_id":"same","turn_id":"two"}),
            ),
            record(
                "response_item",
                json!({"type":"function_call_output","call_id":"same","turn_id":"two","output":"two output"}),
            ),
            record(
                "event_msg",
                json!({"type":"item_completed","turn_id":"two","item":{"type":"CommandExecution","id":"same","command":"two command","aggregated_output":"two output","exit_code":0}}),
            ),
        ] {
            let mut parser = parser(None);
            parser.record(&record("response_item", json!({"type":"function_call","name":"exec_command","arguments":"one arguments","call_id":"same","turn_id":"one"})));
            parser.record(&update);
            // Later updates must not restore either turn or join the two bodies.
            parser.record(&record("response_item", json!({"type":"function_call_output","call_id":"same","turn_id":"one","output":"one output"})));
            let result = parser.finish();
            assert_eq!(result.tools.len(), 1);
            assert!(result.tools[0].turn_id.is_none());
            assert!(result.tools[0].arguments.is_none());
            assert!(result.tools[0].output.is_none());
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|line| line.contains("conflicting explicit turn IDs"))
            );
        }
        let mut selected = parser(Some("two"));
        selected.record(&record("response_item", json!({"type":"function_call","name":"exec_command","arguments":"one arguments","call_id":"same","turn_id":"one"})));
        selected.record(&record("event_msg", json!({"type":"item_completed","turn_id":"two","item":{"type":"CommandExecution","id":"same","command":"two command"}})));
        assert!(selected.finish().tools.is_empty());
    }

    #[test]
    fn session_details_mixed_text_image_file_preserves_body_and_exact_attachment_metadata() {
        let mut parser = parser(Some("one"));
        parser.record(&record("event_msg", json!({"type":"item_completed","thread_id":"thread","turn_id":"one","item":{"type":"UserMessage","id":"mixed","content":[
            {"type":"input_text","text":"inspect these attachments"},
            {"type":"input_image","image_url":"data:image/png;base64,PRIVATE_IMAGE_BLOB","mime_type":"image/png"},
            {"type":"input_file","file_id":"file-1","filename":"input.csv","path":"/recorded/input.csv","mime_type":"text/csv","file_data":"PRIVATE_FILE_BYTES"}
        ]}})));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, "inspect these attachments");
        assert_eq!(result.attachments.len(), 2);
        assert!(
            result
                .attachments
                .iter()
                .all(|attachment| attachment.turn_id.as_deref() == Some("one"))
        );
        assert!(
            result.attachments[0]
                .text
                .contains("Attachment type: input_image")
        );
        assert!(
            result.attachments[0]
                .text
                .contains("data URL bytes omitted")
        );
        assert!(result.attachments[0].text.contains("mime_type: image/png"));
        assert!(result.attachments[1].text.contains("file_id: file-1"));
        assert!(result.attachments[1].text.contains("filename: input.csv"));
        assert!(
            result.attachments[1]
                .text
                .contains("path: /recorded/input.csv")
        );
        let display = result.display_lines().join("\n");
        assert!(!display.contains("PRIVATE_IMAGE_BLOB") && !display.contains("PRIVATE_FILE_BYTES"));
        assert!(
            display.find("Recorded attachment metadata").unwrap()
                < display.find("Messages (1)").unwrap()
        );
    }

    #[test]
    fn session_details_attachment_redaction_future_foreign_and_missingness_are_respected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        let attachment = json!({"type":"message","role":"user","turn_id":"one","content":[{"type":"image","url":"https://recorded.example/image"}]});
        let text = [
            record("session_meta", json!({"id":"thread"})),
            record("response_item", attachment.clone()),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(root.path().join("sessions/rollout-thread.jsonl"), text).unwrap();
        let redacted = load_session_details(root.path(), "thread", None, true);
        assert_eq!(redacted.files_read, 0);
        assert!(redacted.attachments.is_empty());

        let cutoff = DateTime::parse_from_rfc3339("2026-10-04T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut future = DetailParser::new("thread", None, Some(cutoff), SessionDetails::default());
        future.record(&record("session_meta", json!({"id":"thread"})));
        future.record(&json!({"type":"response_item","timestamp":"2026-10-04T03:00:00Z","payload":attachment}));
        assert!(future.finish().attachments.is_empty());

        let mut foreign = DetailParser::new("child", None, None, SessionDetails::default());
        foreign.record(&record(
            "session_meta",
            json!({"id":"child","parent_thread_id":"thread"}),
        ));
        foreign.record(&record("response_item", json!({"type":"message","role":"user","turn_id":"parent-turn","content":[{"type":"file","filename":"inherited.txt"}]})));
        assert!(foreign.finish().attachments.is_empty());

        let mut missing = parser(Some("one"));
        missing.record(&record("response_item", json!({"type":"message","role":"user","turn_id":"two","content":[{"type":"file","filename":"other-turn.txt"}]})));
        missing.record(&record("response_item", json!({"type":"message","role":"user","turn_id":"one","content":[{"type":"attachment"}]})));
        let missing = missing.finish();
        assert_eq!(missing.attachments.len(), 1);
        assert!(missing.attachments[0].text.contains("Metadata: unrecorded"));
        assert!(
            !missing.attachments[0].text.contains("filename:")
                && !missing.attachments[0].text.contains("mime_type:")
        );
    }

    #[test]
    fn session_details_display_builder_bounds_newlines_and_utf8_bytes_including_marker() {
        for body in ["\n".repeat(MAX_TEXT_BYTES), "界".repeat(MAX_TEXT_BYTES / 3)] {
            let result = SessionDetails {
                analysis_lines: vec!["Important derived evidence comes first".into()],
                messages: (0..MAX_CONTENT_BYTES / MAX_TEXT_BYTES)
                    .map(|_| DetailMessage {
                        role: "user".into(),
                        text: body.clone(),
                        timestamp: None,
                        turn_id: None,
                        phase: None,
                    })
                    .collect(),
                ..SessionDetails::default()
            };
            let lines = result.display_lines();
            assert!(lines.len() <= MAX_DISPLAY_LINES);
            assert!(lines.iter().map(|line| line.len() + 1).sum::<usize>() <= MAX_DISPLAY_BYTES);
            assert_eq!(lines.last().map(String::as_str), Some(DISPLAY_TRUNCATION));
            assert_eq!(
                lines.first().map(String::as_str),
                Some("Important derived evidence comes first")
            );
            assert!(lines.iter().all(|line| !line.contains('\n')));
            // The display projection must not mutate the bounded source body.
            assert!(result.messages.iter().all(|message| message.text == body));
        }
        let mut builder = DetailLines::default();
        // Newlines in metadata and analysis lines are bounded too.
        builder.push("\n".repeat(MAX_CONTENT_BYTES));
        let lines = builder.finish();
        assert_eq!(lines.len(), MAX_DISPLAY_LINES);
        assert_eq!(lines.last().map(String::as_str), Some(DISPLAY_TRUNCATION));
        let consumed = std::cell::Cell::new(0);
        let mut builder = DetailLines::default();
        builder.extend(
            std::iter::repeat_with(|| {
                consumed.set(consumed.get() + 1);
                String::new()
            })
            .take(MAX_CONTENT_BYTES),
        );
        assert_eq!(
            consumed.get(),
            MAX_DISPLAY_LINES,
            "building a capped display must not consume the remaining newline-heavy input"
        );
    }

    #[test]
    fn session_details_display_keeps_complete_small_unicode_content() {
        let result = SessionDetails {
            messages: vec![DetailMessage {
                role: "user".into(),
                text: "第一行\nsecond line 🧑‍💻".into(),
                timestamp: None,
                turn_id: Some("one".into()),
                phase: None,
            }],
            ..SessionDetails::default()
        };
        let lines = result.display_lines();
        assert!(lines.iter().any(|line| line == "第一行"));
        assert!(lines.iter().any(|line| line == "second line 🧑‍💻"));
        assert!(!lines.iter().any(|line| line == DISPLAY_TRUNCATION));
    }

    #[test]
    fn session_details_fallback_message_identity_keeps_distinct_times_and_phases() {
        let mut parser = parser(None);
        let first = record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"继续"}),
        );
        let mut later = first.clone();
        later["timestamp"] = json!("2026-10-04T01:01:00Z");
        for record in [&first, &first, &later, &later] {
            parser.record(record);
        }
        let analysis = record(
            "response_item",
            json!({"type":"message","role":"assistant","turn_id":"one","phase":"analysis","content":[{"text":"same text"}]}),
        );
        let mut final_answer = analysis.clone();
        final_answer["payload"]["phase"] = json!("final_answer");
        for record in [&analysis, &analysis, &final_answer, &final_answer] {
            parser.record(record);
        }
        let identified = record(
            "response_item",
            json!({"type":"message","id":"exact-item-id","role":"assistant","turn_id":"one","content":[{"text":"identified"}]}),
        );
        let mut identified_copy = identified.clone();
        identified_copy["timestamp"] = json!("2026-10-04T01:02:00Z");
        parser.record(&identified);
        parser.record(&identified_copy);
        let result = parser.finish();
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|message| message.text == "继续")
                .count(),
            2
        );
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|message| message.text == "same text")
                .count(),
            2
        );
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|message| message.text == "identified")
                .count(),
            1
        );
        assert_eq!(result.messages.len(), 5);
    }
}
