//! Bounded, read-only detail projection of a single local rollout.
//!
//! Conversation content stays in memory and is never added to the history cache.
//! Explicit item/turn/call identities are used instead of time-based attribution.

use std::borrow::Cow;
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
use crate::domain::{
    AgentInteraction, AgentInteractionKind, RolloutDataset, TokenUsage, terminal_safe_text,
};

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
const REQUIRED_DETAIL_LABELS: [&str; 7] = [
    "Codex version",
    "Approval policy",
    "Sandbox policy",
    "Permission profile",
    "Recorded Git branch",
    "Recorded Git commit",
    "Reported context window",
];

/// Limits allocation while building the display, before the UI receives it.
/// A newline-heavy body must not first produce millions of owned strings.
#[derive(Default)]
#[cfg(test)]
struct DetailLines {
    lines: Vec<String>,
    bytes: usize,
    truncated: bool,
}

#[cfg(test)]
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
    analysis_groups: Option<DetailAnalysisGroups>,
    tool_retention: HashMap<String, DetailToolRetention>,
    message_preview_candidates: Vec<DetailMessagePreviewCandidate>,
    preview_reserve: Option<DetailPreviewReserve>,
}

pub(crate) struct MessagePreviewReadTarget<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) turn_id: &'a str,
    pub(crate) preview: Option<&'a str>,
}

struct DetailReadSelection<'a> {
    turn_id: Option<&'a str>,
    preview_requested: bool,
    preview: Option<&'a str>,
}

#[derive(Clone, Debug)]
struct DetailPreviewReserve {
    turn_id: String,
    requested_preview: Option<String>,
    entries: HashMap<[u8; 32], DetailReservedPreview>,
    bytes: usize,
    incomplete: bool,
}

#[derive(Clone, Debug)]
struct DetailReservedPreview {
    preview: String,
    text: Option<String>,
    retention: DetailRetention,
    event_format: bool,
}

impl DetailPreviewReserve {
    fn new(turn_id: &str, preview: Option<&str>) -> Self {
        Self {
            turn_id: turn_id.into(),
            requested_preview: preview
                .filter(|preview| !preview.trim().is_empty())
                .map(str::to_owned),
            entries: HashMap::new(),
            bytes: 0,
            incomplete: false,
        }
    }
}

#[derive(Clone, Debug)]
struct DetailMessagePreviewCandidate {
    turn_id: Option<String>,
    preview: Option<String>,
    original_digest: Option<[u8; 32]>,
    authored_ranges: Vec<std::ops::Range<usize>>,
    message_index: Option<usize>,
    retention: DetailRetention,
    event_format: bool,
}

enum DetailMessageOrigin<'a> {
    EventMessage,
    Item {
        id: Option<&'a str>,
        authored_ranges: Vec<std::ops::Range<usize>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessagePreviewUnavailable {
    ExactTurnRequired,
    Redacted,
    NoMatchingUserMessage,
    Ambiguous,
    RetentionLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MessagePreviewText<'a> {
    Available { text: Cow<'a, str>, truncated: bool },
    Unavailable(MessagePreviewUnavailable),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DetailRetention {
    #[default]
    Complete,
    Truncated,
    Omitted,
}

#[derive(Clone, Copy, Debug, Default)]
struct DetailToolRetention {
    arguments: DetailRetention,
    output: DetailRetention,
}

/// Ranges are recorded while constructing the analysis, never recovered by
/// scanning display labels. They share the existing bounded analysis storage.
#[derive(Clone, Debug)]
struct DetailAnalysisGroups {
    summary: std::ops::Range<usize>,
    models: std::ops::Range<usize>,
    hourly: std::ops::Range<usize>,
}

#[cfg(test)]
pub(crate) struct SessionDetailDisplay {
    pub(crate) lines: Vec<String>,
    pub(crate) tool_header: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionDetailSection {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
    pub children: Vec<SessionDetailSection>,
}

const SECTION_TRUNCATION_ID: &str = "display-truncation";
const SECTION_TRUNCATION_TITLE: &str = "Display truncated";
const SECTION_BODY_OMITTED: &str =
    "[content omitted by the display limit; collapse another section to inspect]";
const SECTION_RESERVED_BYTES: usize =
    SECTION_TRUNCATION_ID.len() + SECTION_TRUNCATION_TITLE.len() + DISPLAY_TRUNCATION.len() + 3;

/// The entire tree shares one display budget, including node titles and IDs.
/// Reserve a final root node so every truncation is visible within that budget.
struct DetailSectionBudget<'a> {
    expanded: &'a HashSet<String>,
    bytes: usize,
    lines: usize,
    truncated: bool,
    identities: HashMap<String, usize>,
    placeholders: HashSet<String>,
}

impl<'a> DetailSectionBudget<'a> {
    fn new(expanded: &'a HashSet<String>) -> Self {
        Self {
            expanded,
            bytes: 0,
            lines: 0,
            truncated: false,
            identities: HashMap::new(),
            placeholders: HashSet::new(),
        }
    }

    fn section(&mut self, id: String, title: &str) -> Option<SessionDetailSection> {
        if self.truncated {
            return None;
        }
        let occurrence = self.identities.entry(id.clone()).or_default();
        *occurrence += 1;
        let id = if *occurrence == 1 {
            id
        } else {
            format!("{id}/copy-{occurrence}")
        };
        let title = terminal_safe_text(&detail_short_title(title, 240));
        let is_expanded = self.expanded.contains(&id);
        let lines = 1 + usize::from(is_expanded);
        let bytes = id.len()
            + title.len()
            + 2
            + if is_expanded {
                SECTION_BODY_OMITTED.len() + 1
            } else {
                0
            };
        if self.lines + lines > MAX_DISPLAY_LINES - 2
            || bytes > MAX_DISPLAY_BYTES.saturating_sub(SECTION_RESERVED_BYTES + self.bytes)
        {
            self.truncated = true;
            return None;
        }
        self.bytes += bytes;
        self.lines += lines;
        if is_expanded {
            self.placeholders.insert(id.clone());
        }
        Some(SessionDetailSection {
            id,
            title,
            lines: if is_expanded {
                vec![SECTION_BODY_OMITTED.into()]
            } else {
                Vec::new()
            },
            children: Vec::new(),
        })
    }

    fn is_expanded(&self, section: &SessionDetailSection) -> bool {
        self.expanded.contains(&section.id)
    }

    fn clear_placeholder(&mut self, section: &mut SessionDetailSection) {
        if self.placeholders.remove(&section.id) {
            section.lines.clear();
            self.bytes -= SECTION_BODY_OMITTED.len() + 1;
            self.lines -= 1;
        }
    }

    fn add_child(&mut self, section: &mut SessionDetailSection, child: SessionDetailSection) {
        self.clear_placeholder(section);
        section.children.push(child);
    }

    fn child(
        &mut self,
        parent: &str,
        identity: &[&str],
        title: &str,
    ) -> Option<SessionDetailSection> {
        if self.truncated {
            return None;
        }
        let mut hash = Sha256::new();
        for part in identity {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        self.section(format!("{parent}/{:x}", hash.finalize()), title)
    }

    fn text(&mut self, section: &mut SessionDetailSection, text: &str) {
        let mut pieces = text.lines();
        let mut touched = false;
        while !self.truncated {
            let Some(line) = pieces.next() else {
                break;
            };
            touched = true;
            self.clear_placeholder(section);
            if self.lines >= MAX_DISPLAY_LINES - 2 {
                self.truncated = true;
                break;
            }
            let available = MAX_DISPLAY_BYTES.saturating_sub(SECTION_RESERVED_BYTES + self.bytes);
            if available <= 1 {
                self.truncated = true;
                break;
            }
            let mut boundary = (available - 1).min(line.len());
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let safe = terminal_safe_text(&line[..boundary]);
            self.bytes += safe.len() + 1;
            self.lines += 1;
            section.lines.push(safe);
            if boundary < line.len() {
                self.truncated = true;
            }
        }
        if touched && self.truncated {
            self.mark_truncated_body(section);
        }
    }

    fn mark_truncated_body(&mut self, section: &mut SessionDetailSection) {
        // Keep a local explanation, including when the retained prefix has
        // only blank lines. Refund the tail before reserving this marker.
        if section.lines.iter().all(|line| line.trim().is_empty()) {
            self.bytes -= section
                .lines
                .iter()
                .map(|line| line.len() + 1)
                .sum::<usize>();
            self.lines -= section.lines.len();
            section.lines.clear();
        }
        let cost = SECTION_BODY_OMITTED.len() + 1;
        loop {
            let available = MAX_DISPLAY_BYTES.saturating_sub(SECTION_RESERVED_BYTES + self.bytes);
            if self.lines < MAX_DISPLAY_LINES - 2 && available >= cost {
                self.bytes += cost;
                self.lines += 1;
                section.lines.push(SECTION_BODY_OMITTED.into());
                break;
            }
            let Some(last) = section.lines.last_mut() else {
                break;
            };
            if self.lines >= MAX_DISPLAY_LINES - 2 {
                self.bytes -= last.len() + 1;
                self.lines -= 1;
                section.lines.pop();
            } else {
                let mut boundary = last.len().saturating_sub(cost - available);
                while !last.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                if boundary == 0 {
                    self.bytes -= last.len() + 1;
                    self.lines -= 1;
                    section.lines.pop();
                } else {
                    self.bytes -= last.len() - boundary;
                    last.truncate(boundary);
                }
            }
        }
    }

    fn body(
        &mut self,
        section: &mut SessionDetailSection,
        text: Option<&str>,
        retention: DetailRetention,
        unavailable: &str,
    ) {
        if self.truncated {
            return;
        }
        match (text, retention) {
            (None, DetailRetention::Omitted) => self.text(
                section,
                "[content not retained: 4,096-record / 2 MiB retention limit]",
            ),
            (None, _) => self.text(section, unavailable),
            (Some(text), DetailRetention::Truncated) => {
                self.text(
                    section,
                    "[retained content truncated: 64 KiB per-field / 2 MiB total limit]",
                );
                if !text.trim().is_empty() {
                    self.text(section, text);
                }
            }
            (Some(text), _) if text.trim().is_empty() => {
                self.text(section, "Recorded content is empty.")
            }
            (Some(text), _) => self.text(section, text),
        }
    }

    fn finish(self, sections: &mut Vec<SessionDetailSection>) {
        if self.truncated {
            sections.push(SessionDetailSection {
                id: SECTION_TRUNCATION_ID.into(),
                title: SECTION_TRUNCATION_TITLE.into(),
                lines: vec![DISPLAY_TRUNCATION.into()],
                children: Vec::new(),
            });
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DetailMessage {
    pub(crate) role: String,
    // Only the exact-turn Message preview can expose retained user content.
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
    /// Match retained user-authored text using an exact turn and the same
    /// normalized 72-character preview used by rollout collection. The rest of
    /// the detail tree remains metadata-only for messages and context.
    pub(crate) fn message_preview_text<'a>(
        &'a self,
        turn_id: Option<&str>,
        preview: Option<&str>,
    ) -> MessagePreviewText<'a> {
        use MessagePreviewUnavailable as Missing;
        if self.redacted {
            return MessagePreviewText::Unavailable(Missing::Redacted);
        }
        let Some(turn_id) = turn_id.filter(|turn| !turn.is_empty()) else {
            return MessagePreviewText::Unavailable(Missing::ExactTurnRequired);
        };
        if self
            .preview_reserve
            .as_ref()
            .is_some_and(|reserve| reserve.turn_id == turn_id && reserve.incomplete)
        {
            return MessagePreviewText::Unavailable(Missing::RetentionLimit);
        }
        let preview = preview.filter(|preview| !preview.trim().is_empty());
        let mut matched_digest = None;
        let mut retained: Option<(Cow<'a, str>, bool, bool)> = None;
        let mut matched = false;
        for candidate in &self.message_preview_candidates {
            if candidate.turn_id.as_deref() != Some(turn_id)
                || candidate.preview.is_none()
                || preview.is_some_and(|preview| candidate.preview.as_deref() != Some(preview))
            {
                continue;
            }
            let Some(digest) = candidate.original_digest else {
                continue;
            };
            if matched_digest.is_some_and(|matched| matched != digest) {
                return MessagePreviewText::Unavailable(Missing::Ambiguous);
            }
            matched_digest = Some(digest);
            matched = true;
            if let Some(message) = candidate
                .message_index
                .and_then(|index| self.messages.get(index))
                .filter(|message| {
                    message.role == "user" && message.turn_id.as_deref() == Some(turn_id)
                })
            {
                let truncated = candidate.retention != DetailRetention::Complete;
                if let Some(text) = retained_authored_text(message, &candidate.authored_ranges)
                    && prefer_message_preview(
                        retained
                            .as_ref()
                            .map(|(text, partial, event)| (text.len(), *partial, *event)),
                        (text.len(), truncated, candidate.event_format),
                    )
                {
                    retained = Some((text, truncated, candidate.event_format));
                }
            }
        }
        if let Some(reserve) = self
            .preview_reserve
            .as_ref()
            .filter(|reserve| reserve.turn_id == turn_id)
        {
            for (digest, candidate) in &reserve.entries {
                if preview.is_some_and(|preview| candidate.preview != preview) {
                    continue;
                }
                if matched_digest.is_some_and(|matched| matched != *digest) {
                    return MessagePreviewText::Unavailable(Missing::Ambiguous);
                }
                matched_digest = Some(*digest);
                matched = true;
                if let Some(text) = candidate.text.as_deref() {
                    let truncated = candidate.retention != DetailRetention::Complete;
                    if prefer_message_preview(
                        retained
                            .as_ref()
                            .map(|(text, partial, event)| (text.len(), *partial, *event)),
                        (text.len(), truncated, candidate.event_format),
                    ) {
                        retained = Some((Cow::Borrowed(text), truncated, candidate.event_format));
                    }
                }
            }
        }
        // Hand-built records (and older in-memory fixtures) have no parser
        // retention evidence. Still require authored text and exact identities.
        let evidenced_indices: HashSet<_> = self
            .message_preview_candidates
            .iter()
            .filter_map(|candidate| candidate.message_index)
            .collect();
        for (index, message) in self.messages.iter().enumerate() {
            if message.role != "user"
                || message.turn_id.as_deref() != Some(turn_id)
                || evidenced_indices.contains(&index)
            {
                continue;
            }
            let Some(text) = user_authored_message_text(&message.text) else {
                continue;
            };
            if preview
                .is_some_and(|preview| normalized_turn_preview(text).as_deref() != Some(preview))
            {
                continue;
            }
            let digest = normalized_authored_digest(text);
            if matched_digest.is_some_and(|matched| matched != digest) {
                return MessagePreviewText::Unavailable(Missing::Ambiguous);
            }
            matched_digest = Some(digest);
            matched = true;
            if prefer_message_preview(
                retained
                    .as_ref()
                    .map(|(text, partial, event)| (text.len(), *partial, *event)),
                (text.len(), false, false),
            ) {
                retained = Some((Cow::Borrowed(text), false, false));
            }
        }
        match retained {
            Some((text, truncated, _)) => MessagePreviewText::Available { text, truncated },
            None if matched => MessagePreviewText::Unavailable(Missing::RetentionLimit),
            None => MessagePreviewText::Unavailable(Missing::NoMatchingUserMessage),
        }
    }

    /// Project only visible branches. Expanded identities are raw section IDs,
    /// without the UI's `recorded.` namespace. Message bodies stay hidden.
    pub(crate) fn detail_sections(&self, expanded: &HashSet<String>) -> Vec<SessionDetailSection> {
        let mut budget = DetailSectionBudget::new(expanded);
        // Fixed roots, and placeholders for expanded roots, precede content.
        // A later exhausted branch must still explain why it has no body.
        let mut sections: Vec<_> = [
            ("analysis", "Analysis / model and hourly usage".to_owned()),
            ("configuration", "Configuration & Git".to_owned()),
            (
                "interactions",
                format!("Agent interactions ({})", self.agent_interactions.len()),
            ),
            (
                "attachments",
                format!("Attachments ({})", self.attachments.len()),
            ),
            ("messages", format!("Messages ({})", self.messages.len())),
            ("tools", format!("Tool calls ({})", self.tools.len())),
            (
                "files",
                format!("File changes ({})", self.file_changes.len()),
            ),
            ("failures", format!("Failures ({})", self.failures.len())),
            (
                "compactions",
                format!("Context compactions ({})", self.compactions.len()),
            ),
            (
                "observations",
                format!("Usage observations ({})", self.usage.len()),
            ),
            ("source", "Source / read warnings".to_owned()),
        ]
        .into_iter()
        .map(|(id, title)| {
            budget
                .section(id.into(), &title)
                .expect("fixed root headings fit the display budget")
        })
        .collect();

        if budget.is_expanded(&sections[10]) {
            budget.text(
                &mut sections[10],
                &format!(
                    "Local source: {} rollout file(s); {} unassigned record(s)",
                    self.files_read, self.unassigned_records
                ),
            );
            budget.text(&mut sections[10], "Only explicit turn / item / call identities determine turn ownership. Unassigned content is excluded from a turn detail.");
            budget.text(&mut sections[10], "Content is loaded on demand, kept in memory, and never fetched from remote sources.");
            for warning in &self.warnings {
                if budget.truncated {
                    break;
                }
                budget.text(&mut sections[10], &format!("Note: {warning}"));
            }
        }
        if self.redacted {
            for section in sections.iter_mut().take(10) {
                if budget.is_expanded(section) {
                    budget.text(section, "redacted; raw rollouts were not opened");
                }
            }
            budget.finish(&mut sections);
            return sections;
        }

        if budget.is_expanded(&sections[1]) {
            for (id, title, is_git) in [
                ("configuration/settings", "Configuration", false),
                ("configuration/git", "Git (recorded and current)", true),
            ] {
                let Some(mut child) = budget.section(id.into(), title) else {
                    break;
                };
                if budget.is_expanded(&child) {
                    for (key, value) in &self.metadata {
                        if budget.truncated {
                            break;
                        }
                        if (key.starts_with("Recorded Git ") || key.starts_with("Current Git "))
                            == is_git
                        {
                            budget.text(&mut child, &format!("{key}: {value}"));
                        }
                    }
                    for label in REQUIRED_DETAIL_LABELS {
                        if label.starts_with("Recorded Git ") == is_git
                            && self.missing_metadata_label(label)
                        {
                            budget.text(&mut child, &format!("{label}: unrecorded"));
                        }
                    }
                }
                budget.add_child(&mut sections[1], child);
            }
        }

        if budget.is_expanded(&sections[0]) {
            if let Some(groups) = &self.analysis_groups {
                for (id, title) in [
                    ("analysis/models", "Model distribution"),
                    ("analysis/hourly", "Hourly token / API trend (UTC)"),
                ] {
                    let Some(child) = budget.section(id.into(), title) else {
                        break;
                    };
                    budget.add_child(&mut sections[0], child);
                }
                for line in self
                    .analysis_lines
                    .get(groups.summary.clone())
                    .unwrap_or(&[])
                {
                    if budget.truncated {
                        break;
                    }
                    budget.text(&mut sections[0], line);
                }
                for (range, child) in [&groups.models, &groups.hourly]
                    .into_iter()
                    .zip(&mut sections[0].children)
                {
                    if !budget.is_expanded(child) {
                        continue;
                    }
                    let lines = self.analysis_lines.get(range.clone()).unwrap_or(&[]);
                    if lines.is_empty() {
                        budget.text(child, "unrecorded / unavailable");
                    }
                    for line in lines {
                        if budget.truncated {
                            break;
                        }
                        budget.text(child, line);
                    }
                }
            } else {
                if self.analysis_lines.is_empty() {
                    budget.text(&mut sections[0], &self.absent_label("Analysis evidence"));
                }
                for line in &self.analysis_lines {
                    if budget.truncated {
                        break;
                    }
                    budget.text(&mut sections[0], line);
                }
            }
        }

        if budget.is_expanded(&sections[2]) {
            budget.text(&mut sections[2], "Child usage is not loaded by this single-thread log projection; an interaction is not a child-usage total.");
            if self.agent_interactions.is_empty() {
                budget.text(
                    &mut sections[2],
                    "No exact call-ID link recorded in the selected local evidence.",
                );
            }
            for interaction in &self.agent_interactions {
                if budget.truncated {
                    break;
                }
                let kind = match interaction.kind {
                    AgentInteractionKind::SpawnStarted => "spawned",
                    AgentInteractionKind::Interacted => "interacted",
                    AgentInteractionKind::Unknown => "unknown",
                };
                let time = time_label(interaction.occurred_at.or(interaction.requested_at));
                let Some(mut child) = budget.child(
                    "interactions",
                    &[
                        &interaction.parent_thread_id,
                        &interaction.parent_turn_id,
                        &interaction.child_thread_id,
                        &interaction.call_id,
                        kind,
                        &time,
                    ],
                    &format!("{kind} child {}", interaction.child_thread_id),
                ) else {
                    break;
                };
                if budget.is_expanded(&child) {
                    budget.text(
                        &mut child,
                        &format!(
                            "Parent thread: {} | {}",
                            interaction.parent_thread_id,
                            turn_label(Some(&interaction.parent_turn_id))
                        ),
                    );
                    budget.text(
                        &mut child,
                        &format!("Call ID: {} | {time}", interaction.call_id),
                    );
                }
                budget.add_child(&mut sections[2], child);
            }
        }

        if budget.is_expanded(&sections[4]) {
            budget.text(&mut sections[4], "Message bodies: hidden");
            if self.messages.is_empty() {
                budget.text(&mut sections[4], &self.absent_label("Message metadata"));
            }
            for message in &self.messages {
                if budget.truncated {
                    break;
                }
                budget.text(
                    &mut sections[4],
                    &format!(
                        "{} | {} | {} | {}",
                        message.role,
                        message.phase.as_deref().unwrap_or("phase unrecorded"),
                        time_label(message.timestamp),
                        turn_label(message.turn_id.as_deref())
                    ),
                );
            }
        }

        if budget.is_expanded(&sections[9]) {
            budget.text(&mut sections[9], "Observations are evidence only; cumulative samples are not summed into the overview.");
            if self.usage.is_empty() {
                budget.text(&mut sections[9], &self.absent_label("Usage evidence"));
            }
            for usage in &self.usage {
                if budget.truncated {
                    break;
                }
                let time = time_label(usage.timestamp);
                let model = usage.model.as_deref().unwrap_or("model unrecorded");
                let tier = usage.service_tier.as_deref().unwrap_or("unrecorded");
                let tokens = format!("{:?}", usage.tokens);
                let exact = if usage.exact {
                    "native request"
                } else {
                    "reported last request; attribution incomplete"
                };
                let Some(mut child) = budget.child(
                    "observations",
                    &[
                        &time,
                        usage.turn_id.as_deref().unwrap_or(""),
                        model,
                        tier,
                        &tokens,
                        exact,
                    ],
                    &format!("{model} | total {} | {time}", usage.tokens.total_tokens),
                ) else {
                    break;
                };
                if budget.is_expanded(&child) {
                    budget.text(
                        &mut child,
                        &format!(
                            "{} | tier {tier} | {exact}",
                            turn_label(usage.turn_id.as_deref())
                        ),
                    );
                    budget.text(&mut child, &format!("Input: {} | cached input: {} | output: {} | reasoning output: {} | total: {}", usage.tokens.input_tokens, usage.tokens.cached_input_tokens, usage.tokens.output_tokens, usage.tokens.reasoning_output_tokens, usage.tokens.total_tokens));
                }
                budget.add_child(&mut sections[9], child);
            }
        }

        if budget.is_expanded(&sections[5]) {
            if self.tools.is_empty() {
                budget.text(
                    &mut sections[5],
                    &self.absent_label("Tool arguments and outputs"),
                );
            }
            for tool in &self.tools {
                if budget.truncated {
                    break;
                }
                let exit = tool
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unrecorded".into());
                let duration = tool
                    .duration_ms
                    .map(|ms| format!("{ms} ms"))
                    .unwrap_or_else(|| "unrecorded".into());
                let name = detail_short_title(&tool.name, 120);
                let Some(mut child) = budget.child(
                    "tools",
                    &[&tool.call_id, tool.turn_id.as_deref().unwrap_or("")],
                    &format!("{name} | Exit: {exit} | Duration: {duration}"),
                ) else {
                    break;
                };
                if budget.is_expanded(&child) {
                    budget.text(
                        &mut child,
                        &format!(
                            "Call ID: {} | {} | {}",
                            tool.call_id,
                            time_label(tool.timestamp),
                            turn_label(tool.turn_id.as_deref())
                        ),
                    );
                    if tool.test_command {
                        budget.text(&mut child, "Command mentions a test runner; inspect result");
                    }
                    // Reserve both leaf headings/placeholders before reading
                    // either body, so a large argument cannot blank the output.
                    for (suffix, title) in [("arguments", "Arguments"), ("output", "Output")] {
                        if let Some(body) = budget.section(format!("{}/{suffix}", child.id), title)
                        {
                            budget.add_child(&mut child, body);
                        }
                    }
                    let retention = self
                        .tool_retention
                        .get(&tool.call_id)
                        .copied()
                        .unwrap_or_default();
                    for (body, (text, state, unavailable)) in child.children.iter_mut().zip([
                        (
                            tool.arguments.as_deref(),
                            retention.arguments,
                            "unrecorded / unavailable in selected local logs",
                        ),
                        (
                            tool.output.as_deref(),
                            retention.output,
                            "unrecorded / not completed in this snapshot",
                        ),
                    ]) {
                        if budget.is_expanded(body) {
                            budget.body(body, text, state, unavailable);
                        }
                    }
                }
                budget.add_child(&mut sections[5], child);
            }
        }

        for (index, label, evidence) in [
            (3, "Attachment", self.attachments.as_slice()),
            (7, "Failure", self.failures.as_slice()),
            (8, "Compaction", self.compactions.as_slice()),
            (6, "Change / recorded diff", self.file_changes.as_slice()),
        ] {
            if !budget.is_expanded(&sections[index]) {
                continue;
            }
            if index == 3 {
                budget.text(
                    &mut sections[3],
                    "Attachment bytes are not opened or downloaded.",
                );
            }
            append_structured_evidence(&mut budget, &mut sections[index], label, evidence);
        }
        budget.finish(&mut sections);
        sections
    }

    #[cfg(test)]
    pub(crate) fn display_lines(
        &self,
        show_message_bodies: bool,
        show_tool_calls: bool,
    ) -> SessionDetailDisplay {
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
        for label in REQUIRED_DETAIL_LABELS {
            if self.missing_metadata_label(label) {
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
        } else if !show_message_bodies {
            lines.push("Message bodies: hidden".to_owned());
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
            if show_message_bodies {
                append_text(&mut lines, &message.text);
            }
            lines.push(String::new());
        }
        let tool_header_index = lines.lines.len();
        lines.push(format!("Tool calls ({})", self.tools.len()));
        let tool_header = (!lines.truncated).then_some(tool_header_index);
        if show_tool_calls {
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
        SessionDetailDisplay {
            lines: lines.finish(),
            tool_header,
        }
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

    fn missing_metadata_label(&self, label: &str) -> bool {
        !self.metadata.keys().any(|key| {
            key.contains(label)
                || (label == "Reported context window" && key.contains("context window"))
        })
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

fn detail_retention(original_bytes: usize, retained: Option<&str>) -> DetailRetention {
    match retained {
        None => DetailRetention::Omitted,
        Some(text) if text.len() < original_bytes => DetailRetention::Truncated,
        Some(_) => DetailRetention::Complete,
    }
}

fn detail_short_title(text: &str, limit: usize) -> String {
    let mut characters = text.chars();
    let mut title: String = characters.by_ref().take(limit).collect();
    if characters.next().is_some() {
        title.push('…');
    }
    title
}

fn append_structured_evidence(
    budget: &mut DetailSectionBudget<'_>,
    section: &mut SessionDetailSection,
    label: &str,
    evidence: &[DetailEvidence],
) {
    if evidence.is_empty() {
        budget.text(section, "unrecorded / unavailable in selected local logs");
    }
    for (index, record) in evidence.iter().enumerate() {
        if budget.truncated {
            break;
        }
        let time = time_label(record.timestamp);
        let turn = turn_label(record.turn_id.as_deref());
        let Some(mut child) = budget.child(
            &section.id,
            &[&time, record.turn_id.as_deref().unwrap_or(""), &record.text],
            &format!("{label} {} | {time} | {turn}", index + 1),
        ) else {
            break;
        };
        if budget.is_expanded(&child) {
            budget.body(
                &mut child,
                Some(&record.text),
                DetailRetention::Complete,
                "unrecorded / unavailable",
            );
        }
        budget.add_child(section, child);
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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
    load_session_details_inner(
        codex_home,
        thread_id,
        DetailReadSelection {
            turn_id,
            preview_requested: false,
            preview: None,
        },
        redact_content,
        starts_at,
        as_of,
        cancelled,
    )
}

pub(crate) fn load_session_details_with_preview_in_range(
    codex_home: &Path,
    target: &MessagePreviewReadTarget<'_>,
    redact_content: bool,
    starts_at: Option<DateTime<Utc>>,
    as_of: Option<DateTime<Utc>>,
    cancelled: &AtomicBool,
) -> SessionDetails {
    load_session_details_inner(
        codex_home,
        target.thread_id,
        DetailReadSelection {
            turn_id: Some(target.turn_id),
            preview_requested: true,
            preview: target.preview,
        },
        redact_content,
        starts_at,
        as_of,
        cancelled,
    )
}

fn load_session_details_inner(
    codex_home: &Path,
    thread_id: &str,
    selection: DetailReadSelection<'_>,
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
    let turn_id = selection.turn_id;
    if selection.preview_requested {
        let Some(turn_id) = turn_id.filter(|turn| !turn.is_empty() && turn.len() <= 1024) else {
            result.warn("An exact local turn identity is required to retain a message preview.");
            return result;
        };
        result.preview_reserve = Some(DetailPreviewReserve::new(turn_id, selection.preview));
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
                            DetailMessageOrigin::Item {
                                id: None,
                                authored_ranges: Vec::new(),
                            },
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
            if text.is_empty() {
                // A remaining byte budget smaller than one UTF-8 character
                // cannot retain a body. Do not label it as a recorded empty body.
                return None;
            }
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
        origin: DetailMessageOrigin<'_>,
    ) {
        if !self.accept_turn(turn.as_deref()) || text.is_empty() {
            return;
        }
        let (id, event_format, authored_ranges) = match origin {
            DetailMessageOrigin::EventMessage => (
                None,
                true,
                user_authored_message_range(&text).into_iter().collect(),
            ),
            DetailMessageOrigin::Item {
                id,
                authored_ranges,
            } => (id, false, authored_ranges),
        };
        if role == "user" {
            self.reserve_message_preview(&text, &authored_ranges, turn.as_deref(), event_format);
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
        let candidate = if role == "user" {
            let authored: Vec<_> = authored_ranges
                .iter()
                .filter_map(|range| text.get(range.clone()))
                .collect();
            Some(DetailMessagePreviewCandidate {
                turn_id: turn.clone(),
                preview: normalized_words_preview(
                    authored.iter().flat_map(|part| part.split_whitespace()),
                ),
                original_digest: (!authored.is_empty()).then(|| {
                    normalized_words_digest(
                        authored.iter().flat_map(|part| part.split_whitespace()),
                    )
                }),
                authored_ranges,
                message_index: None,
                retention: DetailRetention::Omitted,
                event_format,
            })
        } else {
            None
        };
        let original_bytes = text.len();
        let text = self.keep_text(text);
        if let Some(mut candidate) = candidate {
            let raw_retention = detail_retention(original_bytes, text.as_deref());
            let retained_prefix = text.as_ref().map_or(0, |text| {
                if raw_retention == DetailRetention::Truncated {
                    text.strip_suffix("\n[truncated: detail text limit]")
                        .map_or(text.len(), str::len)
                } else {
                    text.len()
                }
            });
            let complete = candidate
                .authored_ranges
                .iter()
                .all(|range| range.end <= retained_prefix);
            candidate.authored_ranges.retain_mut(|range| {
                range.end = range.end.min(retained_prefix);
                range.start < range.end
            });
            candidate.retention = if candidate.authored_ranges.is_empty() {
                DetailRetention::Omitted
            } else if complete {
                DetailRetention::Complete
            } else {
                DetailRetention::Truncated
            };
            candidate.message_index = text.as_ref().map(|_| self.result.messages.len());
            self.result.message_preview_candidates.push(candidate);
        }
        if let Some(text) = text {
            self.result.messages.push(DetailMessage {
                role: role.into(),
                text,
                timestamp: time,
                turn_id: turn,
                phase,
            });
        }
    }

    fn reserve_message_preview(
        &mut self,
        text: &str,
        ranges: &[std::ops::Range<usize>],
        turn_id: Option<&str>,
        event_format: bool,
    ) {
        let Some(reserve) = self
            .result
            .preview_reserve
            .as_mut()
            .filter(|reserve| turn_id == Some(reserve.turn_id.as_str()))
        else {
            return;
        };
        let authored: Vec<_> = ranges
            .iter()
            .filter_map(|range| text.get(range.clone()))
            .collect();
        let Some(preview) =
            normalized_words_preview(authored.iter().flat_map(|part| part.split_whitespace()))
        else {
            return;
        };
        let digest =
            normalized_words_digest(authored.iter().flat_map(|part| part.split_whitespace()));
        if reserve.entries.len() >= MAX_RECORDS && !reserve.entries.contains_key(&digest) {
            reserve.incomplete = true;
            self.result.warn("Targeted message preview reached the 4,096-candidate limit; uniqueness could not be verified.");
            return;
        }
        // Preserve fingerprints for all exact-turn authored candidates so a
        // later no-preview lookup cannot ignore a different user request.
        let requested = reserve
            .requested_preview
            .as_deref()
            .is_none_or(|requested| requested == preview);
        let previous = reserve.entries.get(&digest).map(|entry| {
            (
                entry.text.as_ref().map_or(0, String::len),
                entry.retention,
                entry.event_format,
                entry.text.is_some(),
            )
        });
        let old_bytes = previous.map_or(0, |(bytes, _, _, _)| bytes);
        let available = MAX_TEXT_BYTES.saturating_sub(reserve.bytes - old_bytes);
        let (body, retention) = if requested {
            bounded_authored_text(&authored, available)
        } else {
            (None, DetailRetention::Omitted)
        };
        let new_bytes = body.as_ref().map_or(0, String::len);
        let replace = previous.is_none_or(|(bytes, old_retention, old_event, present)| {
            !present
                || (old_retention == DetailRetention::Truncated
                    && retention == DetailRetention::Complete)
                || (old_retention == DetailRetention::Truncated
                    && retention == DetailRetention::Truncated
                    && new_bytes > bytes)
                || (event_format
                    && !old_event
                    && retention == old_retention
                    && (retention == DetailRetention::Complete || bytes == new_bytes))
        });
        if replace {
            reserve.bytes = reserve.bytes - old_bytes + new_bytes;
            reserve.entries.insert(
                digest,
                DetailReservedPreview {
                    preview,
                    text: body,
                    retention,
                    event_format,
                },
            );
        }
        let saved_retention = reserve.entries.get(&digest).map(|entry| entry.retention);
        if requested && saved_retention != Some(DetailRetention::Complete) {
            self.result.warn("Targeted user-message preview was truncated or omitted at its independent 64 KiB total retained-content limit.");
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
            let authored_ranges = if role == "user" {
                user_message_content_ranges(payload)
            } else {
                Vec::new()
            };
            self.push_message(
                role,
                text,
                turn,
                time,
                string(payload, &["phase"]).map(str::to_owned),
                DetailMessageOrigin::Item {
                    id,
                    authored_ranges,
                },
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
            self.push_message(
                role,
                text.to_owned(),
                turn,
                time,
                None,
                DetailMessageOrigin::EventMessage,
            );
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
                    let authored_ranges = if role == "user" {
                        user_message_content_ranges(item)
                    } else {
                        Vec::new()
                    };
                    self.push_message(
                        role,
                        text,
                        turn,
                        time,
                        string(item, &["phase"]).map(str::to_owned),
                        DetailMessageOrigin::Item {
                            id: string(item, &["id"]),
                            authored_ranges,
                        },
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
        let argument_bytes = arguments.as_ref().map(String::len);
        let arguments = arguments.and_then(|text| self.keep_text(text));
        if self.result.tools.len() >= MAX_RECORDS {
            self.result.warn("The tool-record limit was reached.");
            return;
        }
        if let Some(bytes) = argument_bytes {
            self.result
                .tool_retention
                .entry(call_id.into())
                .or_default()
                .arguments = detail_retention(bytes, arguments.as_deref());
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
        let exit = integer(payload, &["exit_code", "exitCode"])
            .or_else(|| output.as_deref().and_then(output_exit));
        let duration = unsigned(payload, &["duration_ms", "durationMs"])
            .or_else(|| output.as_deref().and_then(output_duration));
        let output_bytes = output.as_ref().map(String::len);
        let output = output.and_then(|text| self.keep_text(text));
        if let Some(bytes) = output_bytes
            && (output.is_some() || self.result.tools[index].output.is_none())
        {
            self.result
                .tool_retention
                .entry(call_id.into())
                .or_default()
                .output = detail_retention(bytes, output.as_deref());
        }
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
            self.result.tool_retention.remove(call_id);
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
        let mut original_indices: Vec<_> = (0..self.result.messages.len()).collect();
        original_indices.sort_by_key(|&index| self.result.messages[index].timestamp);
        let mut new_indices = vec![0; original_indices.len()];
        for (new, old) in original_indices.into_iter().enumerate() {
            new_indices[old] = new;
        }
        for candidate in &mut self.result.message_preview_candidates {
            candidate.message_index = candidate.message_index.map(|old| new_indices[old]);
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

fn user_message_content_ranges(payload: &Value) -> Vec<std::ops::Range<usize>> {
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut offset = 0;
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for part in content {
        let Some(text) = string(part, &["text"]) else {
            continue;
        };
        // Keep offsets for every part in the original content_text join.
        // Only explicit authored types can contribute to the preview body.
        if string(part, &["type"])
            .is_none_or(|kind| matches!(kind, "input_text" | "text" | "InputText" | "Text"))
            && let Some(range) = user_authored_message_range(text)
        {
            let range = range.start + offset..range.end + offset;
            if let Some(previous) = ranges.last_mut()
                && previous.end + 1 == range.start
            {
                // Consecutive authored parts share only the original newline.
                previous.end = range.end;
            } else {
                ranges.push(range);
            }
        }
        offset += text.len() + 1;
    }
    ranges
}

/// Mirror rollout's authored-input filtering without normalizing the body.
/// Preserve its original newlines; only an explicit editor request marker
/// strips the injected preamble, as it does for the collected turn preview.
fn user_authored_message_text(value: &str) -> Option<&str> {
    user_authored_message_range(value).and_then(|range| value.get(range))
}

fn user_authored_message_range(value: &str) -> Option<std::ops::Range<usize>> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || [
            "# AGENTS.md instructions for ",
            "<environment_context>",
            "<codex_internal_context",
            "<turn_aborted>",
            "<recommended_plugins>",
        ]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return None;
    }
    const REQUEST_MARKERS: [&str; 4] = [
        "## My request:",
        "# My request:",
        "## My request for Codex:",
        "# My request for Codex:",
    ];
    let request = REQUEST_MARKERS
        .iter()
        .filter_map(|marker| trimmed.rfind(marker).map(|index| (index, *marker)))
        .max_by_key(|(index, _)| *index)
        .and_then(|(index, marker)| {
            let tail_start = value.len() - value.trim_start().len() + index + marker.len();
            let tail = &value[tail_start..];
            let request = tail.trim();
            let start = tail_start + tail.len() - tail.trim_start().len();
            (!request.is_empty()).then_some(start..start + request.len())
        });
    request.or(Some(0..value.len()))
}

fn normalized_turn_preview(text: &str) -> Option<String> {
    normalized_words_preview(text.split_whitespace())
}

fn normalized_words_preview<'a>(words: impl Iterator<Item = &'a str>) -> Option<String> {
    // title_preview first normalizes whitespace and limits to 96 characters;
    // apply_user_message then takes 69 characters plus "..." when over 72.
    // Reading at most 73 normalized characters produces the same turn value.
    let mut characters = words
        .enumerate()
        .flat_map(|(index, word)| (index > 0).then_some(' ').into_iter().chain(word.chars()));
    let mut preview: String = characters.by_ref().take(72).collect();
    if preview.is_empty() {
        return None;
    }
    if characters.next().is_some() {
        let boundary = preview
            .char_indices()
            .nth(69)
            .map(|(index, _)| index)
            .unwrap_or(preview.len());
        preview.truncate(boundary);
        preview.push_str("...");
    }
    Some(preview)
}

fn normalized_authored_digest(text: &str) -> [u8; 32] {
    normalized_words_digest(text.split_whitespace())
}

fn normalized_words_digest<'a>(words: impl Iterator<Item = &'a str>) -> [u8; 32] {
    // Compare the entire authored body, never the shortened preview. This
    // deduplicates alternate event/response whitespace representations while
    // distinct requests sharing a preview prefix remain ambiguous.
    let mut digest = Sha256::new();
    for (index, word) in words.enumerate() {
        if index > 0 {
            digest.update(b" ");
        }
        digest.update(word.as_bytes());
    }
    digest.finalize().into()
}

fn retained_authored_text<'a>(
    message: &'a DetailMessage,
    ranges: &[std::ops::Range<usize>],
) -> Option<Cow<'a, str>> {
    match ranges {
        [] => None,
        [range] => message.text.get(range.clone()).map(Cow::Borrowed),
        _ => ranges
            .iter()
            .map(|range| message.text.get(range.clone()))
            .collect::<Option<Vec<_>>>()
            .map(|parts| Cow::Owned(parts.join("\n"))),
    }
}

/// Candidate tuples contain retained byte length, partial state, and whether
/// the text preserves the original event format. Completeness wins first;
/// partial copies prefer more retained text, with event format breaking ties.
fn prefer_message_preview(current: Option<(usize, bool, bool)>, next: (usize, bool, bool)) -> bool {
    current.is_none_or(|(old_bytes, old_partial, old_event)| {
        let (bytes, partial, event) = next;
        if partial != old_partial {
            return !partial;
        }
        if partial && bytes != old_bytes {
            return bytes > old_bytes;
        }
        event && !old_event
    })
}

fn bounded_authored_text(parts: &[&str], limit: usize) -> (Option<String>, DetailRetention) {
    let original_bytes = parts
        .iter()
        .fold(0usize, |bytes, part| bytes.saturating_add(part.len()))
        .saturating_add(parts.len().saturating_sub(1));
    let mut body = String::with_capacity(original_bytes.min(limit));
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            if body.len() == limit {
                break;
            }
            body.push('\n');
        }
        let available = limit.saturating_sub(body.len());
        let mut boundary = part.len().min(available);
        while !part.is_char_boundary(boundary) {
            boundary -= 1;
        }
        body.push_str(&part[..boundary]);
        if boundary < part.len() {
            break;
        }
    }
    if body.is_empty() {
        (None, DetailRetention::Omitted)
    } else {
        let retention = if body.len() == original_bytes {
            DetailRetention::Complete
        } else {
            DetailRetention::Truncated
        };
        (Some(body), retention)
    }
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
    let summary_range = 0..lines.len();
    lines.push("Model distribution:".into());
    let model_start = lines.len();
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
    let model_range = model_start..lines.len();
    lines.push("Hourly token / API trend (UTC):".into());
    let hourly_start = lines.len();
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
    result.analysis_groups = Some(DetailAnalysisGroups {
        summary: summary_range,
        models: model_range,
        hourly: hourly_start..lines.len(),
    });
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

    fn expanded_sections(details: &SessionDetails) -> Vec<SessionDetailSection> {
        let mut expanded = HashSet::new();
        for _ in 0..5 {
            let sections = details.detail_sections(&expanded);
            let mut nodes = Vec::new();
            section_nodes(&sections, &mut nodes);
            let before = expanded.len();
            expanded.extend(nodes.iter().map(|node| node.id.clone()));
            if expanded.len() == before {
                return sections;
            }
        }
        details.detail_sections(&expanded)
    }

    fn section_nodes<'a>(
        sections: &'a [SessionDetailSection],
        nodes: &mut Vec<&'a SessionDetailSection>,
    ) {
        for section in sections {
            nodes.push(section);
            section_nodes(&section.children, nodes);
        }
    }

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
    fn session_details_message_preview_matches_exact_turn_and_authored_parts() {
        let mut parser = parser(None);
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"two","message":"first line\nsecond line\t终"}),
        ));
        let mut message = record(
            "response_item",
            json!({"type":"message","id":"selected","role":"user","turn_id":"one","content":[
                {"type":"ContextMarkdown","text":"injected context must stay hidden"},
                {"type":"input_text","text":"<environment_context>injected environment</environment_context>"},
                {"type":"input_text","text":"first line\nsecond line\t终"},
                {"type":"input_image","text":"attachment text must stay hidden"}
            ]}),
        );
        message["timestamp"] = json!("2026-10-04T00:59:00Z");
        parser.record(&message);
        parser.record(&record("response_item", json!({"type":"message","role":"assistant","turn_id":"one","content":[{"text":"assistant secret"}]})));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","message":"unassigned secret"}),
        ));
        let result = parser.finish();
        assert_eq!(
            result.message_preview_text(Some("one"), Some("first line second line 终")),
            MessagePreviewText::Available {
                text: "first line\nsecond line\t终".into(),
                truncated: false,
            }
        );
        assert_eq!(
            result.message_preview_text(Some("one"), Some("another preview")),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::NoMatchingUserMessage)
        );
        assert_eq!(
            result.message_preview_text(None, Some("first line second line 终")),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::ExactTurnRequired)
        );
        let sections = expanded_sections(&result);
        let mut nodes = Vec::new();
        section_nodes(&sections, &mut nodes);
        assert!(
            !nodes
                .iter()
                .flat_map(|node| &node.lines)
                .any(|line| { line.contains("first line") || line.contains("assistant secret") })
        );
    }

    #[test]
    fn session_details_message_preview_rejects_distinct_messages_with_the_same_prefix() {
        let mut parser = parser(Some("one"));
        let prefix = "界".repeat(69);
        for (id, tail) in [
            ("first", "long first request"),
            ("second", "long second request"),
        ] {
            parser.record(&record("response_item", json!({"type":"message","id":id,"role":"user","turn_id":"one","content":[{"type":"input_text","text":format!("{prefix}{tail}")}]})));
        }
        let result = parser.finish();
        let preview = format!("{prefix}...");
        for preview in [Some(preview.as_str()), None] {
            assert_eq!(
                result.message_preview_text(Some("one"), preview),
                MessagePreviewText::Unavailable(MessagePreviewUnavailable::Ambiguous)
            );
        }
    }

    #[test]
    fn session_details_message_preview_deduplicates_full_event_and_response_text() {
        let mut parser = parser(Some("one"));
        let text = "first line\n\nsecond  line\t终...";
        parser.record(&record("response_item", json!({"type":"message","id":"original","role":"user","turn_id":"one","content":[{"type":"input_text","text":text}]})));
        let mut event = record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":text}),
        );
        event["timestamp"] = json!("2026-10-04T01:00:01Z");
        parser.record(&event);
        let result = parser.finish();
        assert_eq!(result.messages.len(), 2);
        for preview in [Some("first line second line 终..."), None] {
            assert_eq!(
                result.message_preview_text(Some("one"), preview),
                MessagePreviewText::Available {
                    text: text.into(),
                    truncated: false
                }
            );
        }
        assert_eq!(
            normalized_turn_preview(&"x".repeat(72)),
            Some("x".repeat(72))
        );
        assert_eq!(
            normalized_turn_preview(&"x".repeat(73)),
            Some(format!("{}...", "x".repeat(69)))
        );
    }

    #[test]
    fn session_details_message_preview_prefers_event_format_for_split_response_copies() {
        let mut parser = parser(Some("one"));
        parser.record(&record(
            "response_item",
            json!({"type":"message","id":"original","role":"user","turn_id":"one","content":[
                {"type":"input_text","text":"first line"},
                {"type":"input_text","text":"second line"}
            ]}),
        ));
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"first line  second line"}),
        ));
        let result = parser.finish();
        for preview in [Some("first line second line"), None] {
            assert_eq!(
                result.message_preview_text(Some("one"), preview),
                MessagePreviewText::Available {
                    text: "first line  second line".into(),
                    truncated: false,
                }
            );
        }
    }

    #[test]
    fn session_details_message_preview_reports_truncated_and_omitted_retention() {
        let mut parser = parser(None);
        let long_text = "界".repeat(MAX_TEXT_BYTES);
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"truncated","message":long_text}),
        ));
        parser.content_bytes = MAX_CONTENT_BYTES;
        parser.record(&record("event_msg", json!({"type":"user_message","turn_id":"omitted","message":"this request was not retained"})));
        let result = parser.finish();
        let preview = normalized_turn_preview(&long_text).unwrap();
        let MessagePreviewText::Available { text, truncated } =
            result.message_preview_text(Some("truncated"), Some(&preview))
        else {
            panic!("the exact truncated request must remain selectable");
        };
        assert!(truncated);
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert!(!text.contains("[truncated: detail text limit]"));
        assert!(
            result.messages[0]
                .text
                .ends_with("[truncated: detail text limit]")
        );
        assert_eq!(
            result.message_preview_text(Some("omitted"), Some("this request was not retained")),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::RetentionLimit)
        );
        assert_eq!(
            result.message_preview_text(Some("omitted"), None),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::RetentionLimit)
        );
        assert!(!result.warnings.is_empty());
    }

    #[test]
    fn session_details_message_preview_no_preview_still_requires_one_authored_request() {
        let mut parser = parser(Some("one"));
        parser.record(&record("event_msg", json!({"type":"user_message","turn_id":"one","message":"# AGENTS.md instructions for /project\nprivate instructions"})));
        parser.record(&record("event_msg", json!({"type":"user_message","turn_id":"one","message":"editor context\n## My request:\nactual request\nwith details"})));
        let result = parser.finish();
        assert_eq!(
            result.message_preview_text(Some("one"), None),
            MessagePreviewText::Available {
                text: "actual request\nwith details".into(),
                truncated: false,
            }
        );
        let mut redacted = result;
        redacted.redacted = true;
        assert_eq!(
            redacted.message_preview_text(Some("one"), None),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::Redacted)
        );
    }

    #[test]
    fn session_details_message_preview_preserves_raw_identity_counts_and_shared_budget() {
        let mut parser = parser(Some("one"));
        let synthetic = "# AGENTS.md instructions for /project\nprivate instructions";
        let first = "first editor preamble\n## My request:\nactual request\nwith details";
        let second = "second editor preamble\n## My request:\nactual request\nwith details";
        for text in [synthetic, synthetic, first, second] {
            parser.record(&record(
                "event_msg",
                json!({"type":"user_message","turn_id":"one","message":text}),
            ));
        }
        assert_eq!(parser.result.messages.len(), 3);
        assert_eq!(parser.seen.len(), 3);
        assert_eq!(parser.record_count, 3);
        assert_eq!(
            parser.content_bytes,
            synthetic.len() + first.len() + second.len()
        );
        let result = parser.finish();
        assert_eq!(
            result
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            [synthetic, first, second]
        );
        assert_eq!(
            result.message_preview_text(Some("one"), None),
            MessagePreviewText::Available {
                text: "actual request\nwith details".into(),
                truncated: false,
            }
        );

        let mut parser = self::parser(Some("one"));
        parser.content_bytes = MAX_CONTENT_BYTES - synthetic.len();
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":synthetic}),
        ));
        parser.record(&record("response_item", json!({"type":"function_call","call_id":"later","turn_id":"one","name":"exec","arguments":"must still share the original retention budget"})));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, synthetic);
        assert!(result.tools[0].arguments.is_none());
        assert_eq!(
            result.tool_retention["later"].arguments,
            DetailRetention::Omitted
        );
    }

    #[test]
    fn session_details_message_preview_context_only_keeps_metadata_without_fallback_body() {
        let mut parser = parser(Some("one"));
        let text = "ordinary-looking injected context";
        parser.record(&record("response_item", json!({"type":"message","id":"context-only","role":"user","turn_id":"one","content":[{"type":"ContextMarkdown","text":text}]})));
        let result = parser.finish();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].text, text);
        for preview in [Some(text), None] {
            assert_eq!(
                result.message_preview_text(Some("one"), preview),
                MessagePreviewText::Unavailable(MessagePreviewUnavailable::NoMatchingUserMessage)
            );
        }
    }

    #[test]
    fn session_details_message_preview_disjoint_authored_ranges_join_without_context() {
        let mut parser = parser(Some("one"));
        parser.record(&record(
            "response_item",
            json!({"type":"message","id":"mixed","role":"user","turn_id":"one","content":[
                {"type":"input_text","text":"first line\nfirst details"},
                {"type":"ContextMarkdown","text":"context must stay hidden"},
                {"type":"input_text","text":"second line\nsecond details"}
            ]}),
        ));
        let original =
            "first line\nfirst details\ncontext must stay hidden\nsecond line\nsecond details";
        assert_eq!(parser.content_bytes, original.len());
        let result = parser.finish();
        assert_eq!(result.messages[0].text, original);
        let MessagePreviewText::Available { text, truncated } = result.message_preview_text(
            Some("one"),
            Some("first line first details second line second details"),
        ) else {
            panic!("authored fragments must remain available");
        };
        assert!(!truncated);
        assert!(matches!(&text, Cow::Owned(_)));
        assert_eq!(
            text,
            "first line\nfirst details\nsecond line\nsecond details"
        );
    }

    #[test]
    fn session_details_message_preview_ranges_respect_original_raw_field_truncation() {
        for (prefix, suffix, expected) in [
            (
                "",
                "selected request",
                MessagePreviewText::Unavailable(MessagePreviewUnavailable::RetentionLimit),
            ),
            (
                "first request",
                "last request",
                MessagePreviewText::Available {
                    text: "first request".into(),
                    truncated: true,
                },
            ),
            (
                "first request",
                "",
                MessagePreviewText::Available {
                    text: "first request".into(),
                    truncated: false,
                },
            ),
        ] {
            let mut parser = parser(Some("one"));
            parser.record(&record(
                "response_item",
                json!({"type":"message","id":"mixed","role":"user","turn_id":"one","content":[
                    {"type":"input_text","text":prefix},
                    {"type":"ContextMarkdown","text":"c".repeat(MAX_TEXT_BYTES + 32)},
                    {"type":"input_text","text":suffix}
                ]}),
            ));
            assert_eq!(parser.content_bytes, MAX_TEXT_BYTES);
            let result = parser.finish();
            assert_eq!(result.messages.len(), 1);
            assert!(
                result.messages[0]
                    .text
                    .ends_with("[truncated: detail text limit]")
            );
            assert_eq!(result.message_preview_text(Some("one"), None), expected);
        }
    }

    #[test]
    fn session_details_targeted_preview_reader_preserves_old_shared_budget_and_recovers_short_request()
     {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        let mut records = vec![record("session_meta", json!({"id":"thread"}))];
        for index in 0..32 {
            records.push(record("response_item", json!({"type":"function_call","call_id":format!("other-{index}"),"turn_id":"other","name":"exec","arguments":"x".repeat(MAX_TEXT_BYTES)})));
        }
        let text = "selected user's complete request\nwith its original newlines";
        records.push(record("event_msg", json!({"type":"item_completed","turn_id":"one","item":{"type":"UserMessage","id":"selected","content":[{"type":"text","text":text}]}})));
        records.push(record("response_item", json!({"type":"function_call","call_id":"late","turn_id":"one","name":"exec","arguments":"late arguments still share the old budget"})));
        let content = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(directory.join("thread.jsonl"), content).unwrap();
        let preview = normalized_turn_preview(text).unwrap();
        let old = load_session_details(root.path(), "thread", Some("one"), false);
        let target = MessagePreviewReadTarget {
            thread_id: "thread",
            turn_id: "one",
            preview: Some(&preview),
        };
        let new = load_session_details_with_preview_in_range(
            root.path(),
            &target,
            false,
            None,
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(
            old.message_preview_text(Some("one"), Some(&preview)),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::RetentionLimit)
        );
        assert_eq!(
            new.message_preview_text(Some("one"), Some(&preview)),
            MessagePreviewText::Available {
                text: text.into(),
                truncated: false
            }
        );
        assert!(old.preview_reserve.is_none());
        assert_eq!(new.preview_reserve.as_ref().unwrap().bytes, text.len());
        assert_eq!(old.messages.len(), new.messages.len());
        assert!(new.messages.is_empty());
        assert_eq!(old.tools.len(), new.tools.len());
        assert_eq!(new.tools.len(), 1);
        assert!(old.tools[0].arguments.is_none());
        assert!(new.tools[0].arguments.is_none());
        assert_eq!(
            old.tool_retention["late"].arguments,
            new.tool_retention["late"].arguments
        );
    }

    #[test]
    fn session_details_targeted_preview_reader_obeys_exact_preview_owner_range_and_redaction() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("sessions");
        std::fs::create_dir(&directory).unwrap();
        let mut records = vec![record("session_meta", json!({"id":"thread"}))];
        records.push(record(
            "event_msg",
            json!({"type":"user_message","turn_id":"other","message":"selected request"}),
        ));
        records.push(record(
            "event_msg",
            json!({"type":"user_message","message":"selected request"}),
        ));
        records.push(record("response_item", json!({"type":"message","turn_id":"one","role":"assistant","content":[{"text":"selected request"}]})));
        records.push(record("response_item", json!({"type":"message","turn_id":"one","role":"user","content":[{"type":"ContextMarkdown","text":"selected request"}]})));
        records.push(record("event_msg", json!({"type":"user_message","turn_id":"one","message":"<environment_context>selected request</environment_context>"})));
        let mut old = record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"selected request"}),
        );
        old["timestamp"] = json!("2026-10-04T00:00:00Z");
        records.push(old);
        let mut future = record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"selected request"}),
        );
        future["timestamp"] = json!("2026-10-04T02:00:00Z");
        records.push(future);
        records.push(record("session_meta", json!({"id":"parent"})));
        records.push(record("response_item", json!({"type":"message","turn_id":"one","role":"user","content":[{"type":"input_text","text":"selected request"}]})));
        records.push(record("session_meta", json!({"id":"thread"})));
        records.push(record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"actual authored request"}),
        ));
        std::fs::write(
            directory.join("thread.jsonl"),
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let starts_at = Some(timestamp(&json!("2026-10-04T00:59:00Z")).unwrap());
        let as_of = Some(timestamp(&json!("2026-10-04T01:00:00Z")).unwrap());
        let target = MessagePreviewReadTarget {
            thread_id: "thread",
            turn_id: "one",
            preview: Some("selected request"),
        };
        let details = load_session_details_with_preview_in_range(
            root.path(),
            &target,
            false,
            starts_at,
            as_of,
            &AtomicBool::new(false),
        );
        assert_eq!(
            details.message_preview_text(Some("one"), target.preview),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::NoMatchingUserMessage)
        );
        let reserve = details.preview_reserve.as_ref().unwrap();
        assert_eq!(reserve.bytes, 0);
        assert_eq!(reserve.entries.len(), 1);
        assert!(reserve.entries.values().all(|entry| entry.text.is_none()));
        assert_eq!(
            details.message_preview_text(Some("other"), target.preview),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::NoMatchingUserMessage)
        );
        let redacted = load_session_details_with_preview_in_range(
            root.path(),
            &target,
            true,
            starts_at,
            as_of,
            &AtomicBool::new(false),
        );
        assert_eq!(redacted.files_read, 0);
        assert!(redacted.preview_reserve.is_none());
        assert_eq!(
            redacted.message_preview_text(Some("one"), target.preview),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::Redacted)
        );
    }

    #[test]
    fn session_details_targeted_preview_ignores_exhausted_shared_seen_and_record_limits() {
        let mut parser = parser(Some("one"));
        parser.result.preview_reserve =
            Some(DetailPreviewReserve::new("one", Some("selected request")));
        parser
            .seen
            .extend((0..MAX_RECORDS).map(|index| format!("already-seen-{index}")));
        parser.record_count = MAX_RECORDS;
        parser.content_bytes = MAX_CONTENT_BYTES;
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":"selected request"}),
        ));
        assert_eq!(parser.content_bytes, MAX_CONTENT_BYTES);
        assert_eq!(parser.record_count, MAX_RECORDS);
        assert_eq!(parser.seen.len(), MAX_RECORDS);
        let details = parser.finish();
        assert!(details.messages.is_empty());
        assert_eq!(
            details.message_preview_text(Some("one"), Some("selected request")),
            MessagePreviewText::Available {
                text: "selected request".into(),
                truncated: false
            }
        );
    }

    #[test]
    fn session_details_targeted_preview_total_cap_deduplicates_and_keeps_ambiguity_evidence() {
        let mut parser = parser(Some("one"));
        parser.result.preview_reserve = Some(DetailPreviewReserve::new("one", None));
        parser.content_bytes = MAX_CONTENT_BYTES;
        for (turn, text) in [
            ("one", "first line\nsecond line"),
            ("one", "first line second line"),
            ("other", "unrelated request"),
        ] {
            parser.record(&record(
                "event_msg",
                json!({"type":"user_message","turn_id":turn,"message":text}),
            ));
        }
        let reserve = parser.result.preview_reserve.as_ref().unwrap();
        assert_eq!(reserve.entries.len(), 1);
        assert_eq!(reserve.bytes, "first line\nsecond line".len());
        let long_text = "界".repeat(MAX_TEXT_BYTES);
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":long_text}),
        ));
        let reserve = parser.result.preview_reserve.as_ref().unwrap();
        assert!(reserve.bytes <= MAX_TEXT_BYTES);
        assert_eq!(
            reserve.bytes,
            reserve
                .entries
                .values()
                .map(|entry| entry.text.as_ref().map_or(0, String::len))
                .sum::<usize>()
        );
        let details = parser.finish();
        let long_preview = normalized_turn_preview(&long_text).unwrap();
        let MessagePreviewText::Available { text, truncated } =
            details.message_preview_text(Some("one"), Some(&long_preview))
        else {
            panic!("the bounded authored prefix must remain inspectable");
        };
        assert!(truncated);
        assert!(!text.is_empty());
        assert!(text.len() < MAX_TEXT_BYTES);
        assert!(text.chars().all(|ch| ch == '界'));
        assert_eq!(
            details.message_preview_text(Some("one"), None),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::Ambiguous)
        );

        let mut parser = self::parser(Some("one"));
        let prefix = "x".repeat(69);
        let preview = format!("{prefix}...");
        parser.result.preview_reserve = Some(DetailPreviewReserve::new("one", Some(&preview)));
        parser.content_bytes = MAX_CONTENT_BYTES;
        for suffix in ["different first tail", "different second tail"] {
            parser.record(&record("event_msg", json!({"type":"user_message","turn_id":"one","message":format!("{prefix}{suffix}")})));
        }
        assert_eq!(
            parser
                .finish()
                .message_preview_text(Some("one"), Some(&preview)),
            MessagePreviewText::Unavailable(MessagePreviewUnavailable::Ambiguous)
        );
    }

    #[test]
    fn session_details_targeted_preview_event_format_replacement_refunds_budget_and_keeps_complete_text()
     {
        for (response, event, exhaust_remaining) in [
            ("first\nsecond", "first  second", false),
            ("first  \nsecond", "first second", false),
            ("first\nsecond", "first                      second", true),
        ] {
            let mut parser = parser(Some("one"));
            parser.result.preview_reserve = Some(DetailPreviewReserve::new("one", None));
            parser.content_bytes = MAX_CONTENT_BYTES;
            parser.record(&record("response_item", json!({"type":"message","id":"original","turn_id":"one","role":"user","content":[{"type":"input_text","text":response}]})));
            if exhaust_remaining {
                let remaining =
                    MAX_TEXT_BYTES - parser.result.preview_reserve.as_ref().unwrap().bytes;
                parser.record(&record(
                    "event_msg",
                    json!({"type":"user_message","turn_id":"one","message":"z".repeat(remaining)}),
                ));
            }
            parser.record(&record(
                "event_msg",
                json!({"type":"user_message","turn_id":"one","message":event}),
            ));
            let reserve = parser.result.preview_reserve.as_ref().unwrap();
            assert!(reserve.bytes <= MAX_TEXT_BYTES);
            assert_eq!(
                reserve.bytes,
                reserve
                    .entries
                    .values()
                    .map(|entry| entry.text.as_ref().map_or(0, String::len))
                    .sum::<usize>()
            );
            let expected = if exhaust_remaining { response } else { event };
            if !exhaust_remaining {
                assert_eq!(reserve.bytes, event.len());
            }
            let details = parser.finish();
            assert_eq!(
                details.message_preview_text(Some("one"), Some("first second")),
                MessagePreviewText::Available {
                    text: expected.into(),
                    truncated: false
                }
            );
            assert!(!details.warnings.iter().any(|warning| {
                warning.contains("Targeted user-message preview was truncated or omitted")
            }));
        }
    }

    #[test]
    fn session_details_targeted_preview_prefers_longer_reserved_prefix_over_short_shared_prefix() {
        let mut parser = parser(Some("one"));
        let text = "x".repeat(MAX_TEXT_BYTES + 100);
        let preview = normalized_turn_preview(&text).unwrap();
        parser.result.preview_reserve = Some(DetailPreviewReserve::new("one", Some(&preview)));
        parser.content_bytes = MAX_CONTENT_BYTES - 64;
        parser.record(&record(
            "event_msg",
            json!({"type":"user_message","turn_id":"one","message":text}),
        ));
        let details = parser.finish();
        assert_eq!(details.messages.len(), 1);
        assert_eq!(details.messages[0].text.len(), 64);
        let MessagePreviewText::Available { text, truncated } =
            details.message_preview_text(Some("one"), Some(&preview))
        else {
            panic!("the longest retained authored prefix must be available");
        };
        assert!(truncated);
        assert_eq!(text.len(), MAX_TEXT_BYTES);
        assert!(text.chars().all(|ch| ch == 'x'));
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
        assert!(
            !result
                .display_lines(true, true)
                .lines
                .join("\n")
                .contains("secret")
        );
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
        let analysis = &expanded_sections(&result)[0];
        assert!(
            analysis
                .lines
                .iter()
                .any(|line| line.contains("100 / 1000 tokens (10.0%)"))
        );
        assert_eq!(analysis.children.len(), 2);
        assert_eq!(analysis.children[0].id, "analysis/models");
        assert_eq!(analysis.children[1].id, "analysis/hourly");
        assert!(
            analysis.children[0]
                .lines
                .iter()
                .any(|line| line.contains("gpt-5.6-sol: 120 tokens"))
        );
        assert!(
            analysis.children[1]
                .lines
                .iter()
                .any(|line| line.contains("2026-10-04 01:00 UTC: 120 tokens"))
        );
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
        let display = result.display_lines(true, true).lines.join("\n");
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
            let display = result.display_lines(true, true);
            assert!(display.tool_header.is_none());
            let lines = display.lines;
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
        let lines = result.display_lines(true, true).lines;
        assert!(lines.iter().any(|line| line == "第一行"));
        assert!(lines.iter().any(|line| line == "second line 🧑‍💻"));
        assert!(!lines.iter().any(|line| line == DISPLAY_TRUNCATION));
    }

    #[test]
    fn session_details_display_collapses_tools_without_hiding_later_evidence() {
        let result = SessionDetails {
            messages: vec![DetailMessage {
                role: "user".into(),
                text: "retained message body".into(),
                timestamp: None,
                turn_id: Some("one".into()),
                phase: None,
            }],
            tools: vec![DetailTool {
                call_id: "retained-call-id".into(),
                name: "retained-tool-name".into(),
                arguments: Some("retained arguments".into()),
                output: Some("retained output\nsecond output line".into()),
                exit_code: Some(7),
                duration_ms: Some(120),
                timestamp: None,
                turn_id: Some("one".into()),
                test_command: true,
            }],
            file_changes: vec![DetailEvidence {
                text: "retained recorded diff".into(),
                timestamp: None,
                turn_id: Some("one".into()),
            }],
            usage: vec![DetailUsage {
                timestamp: None,
                turn_id: Some("one".into()),
                model: Some("retained-usage-model".into()),
                service_tier: None,
                tokens: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    total_tokens: 5,
                    ..TokenUsage::default()
                },
                exact: true,
            }],
            ..SessionDetails::default()
        };
        let collapsed = result.display_lines(false, false);
        let header = collapsed.tool_header.expect("tool header remains visible");
        assert_eq!(collapsed.lines[header], "Tool calls (1)");
        let collapsed_text = collapsed.lines.join("\n");
        for hidden in [
            "retained message body",
            "retained-tool-name",
            "retained-call-id",
            "retained arguments",
            "retained output",
            "Exit: 7",
        ] {
            assert!(!collapsed_text.contains(hidden));
        }
        assert!(collapsed_text.contains("Message bodies: hidden"));
        assert!(
            collapsed.lines[header + 1..]
                .iter()
                .any(|line| line == "retained recorded diff")
        );
        assert!(
            collapsed.lines[header + 1..]
                .iter()
                .any(|line| line.contains("retained-usage-model") && line.contains("total 5"))
        );

        let expanded = result.display_lines(true, true);
        assert_eq!(
            expanded.lines[expanded.tool_header.unwrap()],
            "Tool calls (1)"
        );
        let expanded_text = expanded.lines.join("\n");
        for visible in [
            "retained message body",
            "retained-tool-name",
            "retained-call-id",
            "retained arguments",
            "retained output\nsecond output line",
            "Exit: 7 | Duration: 120 ms",
        ] {
            assert!(expanded_text.contains(visible));
        }
        assert_eq!(result.messages[0].text, "retained message body");
        assert_eq!(
            result.tools[0].arguments.as_deref(),
            Some("retained arguments")
        );
        assert_eq!(
            result.tools[0].output.as_deref(),
            Some("retained output\nsecond output line")
        );
        assert_eq!(result.tools[0].exit_code, Some(7));
        assert_eq!(result.tools[0].duration_ms, Some(120));

        let empty = SessionDetails::default();
        let collapsed = empty.display_lines(true, false);
        assert_eq!(
            collapsed.lines[collapsed.tool_header.unwrap()],
            "Tool calls (0)"
        );
        assert!(
            !collapsed
                .lines
                .iter()
                .any(|line| line.starts_with("Tool arguments and outputs:"))
        );
        assert!(
            empty
                .display_lines(true, true)
                .lines
                .iter()
                .any(|line| line.starts_with("Tool arguments and outputs:"))
        );
    }

    #[test]
    fn session_details_sections_use_typed_hierarchy_and_stable_unique_identities() {
        let tool = DetailTool {
            call_id: "same-call".into(),
            name: "exec\u{1b}\u{202e}command".into(),
            arguments: Some("cargo\ttest\nTool calls (999)".into()),
            output: Some("Output:\nrecorded result".into()),
            exit_code: Some(7),
            duration_ms: Some(120),
            timestamp: None,
            turn_id: Some("one".into()),
            test_command: true,
        };
        let mut other_turn = tool.clone();
        other_turn.turn_id = Some("two".into());
        let result = SessionDetails {
            messages: vec![DetailMessage {
                role: "user".into(),
                text: "PRIVATE MESSAGE BODY".into(),
                timestamp: None,
                turn_id: Some("one".into()),
                phase: Some("question".into()),
            }],
            tools: vec![tool.clone(), other_turn, tool],
            metadata: BTreeMap::from([
                ("Model".into(), "recorded-model".into()),
                ("Recorded Git commit".into(), "historical-commit".into()),
                ("Current Git commit".into(), "current-commit".into()),
            ]),
            file_changes: vec![DetailEvidence {
                text: "recorded diff".into(),
                timestamp: None,
                turn_id: Some("one".into()),
            }],
            usage: vec![DetailUsage {
                timestamp: None,
                turn_id: Some("one".into()),
                model: Some("recorded-model".into()),
                service_tier: None,
                tokens: TokenUsage {
                    total_tokens: 5,
                    ..TokenUsage::default()
                },
                exact: false,
            }],
            ..SessionDetails::default()
        };
        let sections = expanded_sections(&result);
        assert_eq!(sections, expanded_sections(&result));
        let mut nodes = Vec::new();
        section_nodes(&sections, &mut nodes);
        let ids: HashSet<_> = nodes.iter().map(|node| &node.id).collect();
        assert_eq!(ids.len(), nodes.len());
        assert!(nodes.iter().all(|node| {
            !node.title.contains("PRIVATE MESSAGE BODY")
                && node
                    .lines
                    .iter()
                    .all(|line| !line.contains("PRIVATE MESSAGE BODY"))
        }));
        assert!(nodes.iter().all(|node| {
            node.title
                .chars()
                .all(|ch| !ch.is_control() && ch != '\u{202e}')
                && node
                    .lines
                    .iter()
                    .all(|line| line.chars().all(|ch| !ch.is_control() && ch != '\u{202e}'))
        }));
        let configuration = &sections[1];
        assert_eq!(configuration.children[0].id, "configuration/settings");
        assert!(
            configuration.children[0]
                .lines
                .iter()
                .any(|line| line == "Model: recorded-model")
        );
        for label in [
            "Approval policy",
            "Sandbox policy",
            "Reported context window",
        ] {
            assert!(
                configuration.children[0]
                    .lines
                    .iter()
                    .any(|line| line == &format!("{label}: unrecorded"))
            );
        }
        assert_eq!(configuration.children[1].id, "configuration/git");
        assert!(
            configuration.children[1]
                .lines
                .iter()
                .any(|line| line == "Recorded Git commit: historical-commit")
        );
        assert!(
            configuration.children[1]
                .lines
                .iter()
                .any(|line| line == "Current Git commit: current-commit")
        );
        let tools = &sections[5];
        assert_eq!(tools.children.len(), 3);
        assert!(
            tools.lines.is_empty(),
            "tool summaries are child titles, not duplicate parent rows"
        );
        assert!(
            tools
                .children
                .iter()
                .all(|child| child.title.contains("Exit: 7 | Duration: 120 ms"))
        );
        for child in &tools.children {
            assert_eq!(child.children.len(), 2);
            assert_eq!(child.children[0].id, format!("{}/arguments", child.id));
            assert_eq!(child.children[0].lines, ["cargo test", "Tool calls (999)"]);
            assert_eq!(child.children[1].id, format!("{}/output", child.id));
            assert_eq!(child.children[1].lines, ["Output:", "recorded result"]);
        }
        assert_eq!(sections[6].children[0].lines, ["recorded diff"]);
        assert!(
            sections[9].children[0]
                .lines
                .iter()
                .any(|line| line.contains("attribution incomplete"))
        );
        let mut updated = result.clone();
        updated.tools[0].output = Some("later output".into());
        assert_eq!(
            expanded_sections(&updated)[5].children[0].id,
            tools.children[0].id
        );
        assert_eq!(
            result.tools[0].arguments.as_deref(),
            Some("cargo\ttest\nTool calls (999)")
        );
        assert_eq!(result.messages[0].text, "PRIVATE MESSAGE BODY");

        let redacted = SessionDetails {
            redacted: true,
            ..result
        };
        let mut nodes = Vec::new();
        let redacted_sections = expanded_sections(&redacted);
        section_nodes(&redacted_sections, &mut nodes);
        assert!(nodes.iter().all(|node| node.lines.iter().all(|line| {
            !line.contains("recorded result") && !line.contains("PRIVATE MESSAGE BODY")
        })));
    }

    #[test]
    fn session_details_sections_share_bounds_and_keep_source_and_configuration_before_bodies() {
        for (body, oversized_analysis) in [
            ("x\n".repeat(MAX_TEXT_BYTES / 2), false),
            ("界".repeat(MAX_TEXT_BYTES / 3), false),
            (String::new(), true),
        ] {
            let result = SessionDetails {
                metadata: BTreeMap::from([("Codex version".into(), "recorded-version".into())]),
                analysis_lines: if oversized_analysis {
                    vec!["x\n".repeat(MAX_CONTENT_BYTES / 2)]
                } else {
                    Vec::new()
                },
                tools: (0..MAX_CONTENT_BYTES / MAX_TEXT_BYTES)
                    .map(|index| DetailTool {
                        call_id: index.to_string(),
                        name: "exec_command".into(),
                        arguments: None,
                        output: Some(body.clone()),
                        exit_code: None,
                        duration_ms: None,
                        timestamp: None,
                        turn_id: None,
                        test_command: false,
                    })
                    .collect(),
                warnings: vec!["retained read warning".into()],
                ..SessionDetails::default()
            };
            let sections = expanded_sections(&result);
            assert!(
                sections[1].children[0]
                    .lines
                    .iter()
                    .any(|line| line == "Codex version: recorded-version")
            );
            assert!(
                sections[10]
                    .lines
                    .iter()
                    .any(|line| line == "Note: retained read warning")
            );
            assert_eq!(sections.last().unwrap().id, SECTION_TRUNCATION_ID);
            assert_eq!(sections.last().unwrap().lines, [DISPLAY_TRUNCATION]);
            let mut nodes = Vec::new();
            section_nodes(&sections, &mut nodes);
            let total_lines: usize = nodes.iter().map(|node| 1 + node.lines.len()).sum();
            let total_bytes: usize = nodes
                .iter()
                .map(|node| {
                    node.id.len()
                        + node.title.len()
                        + 2
                        + node.lines.iter().map(|line| line.len() + 1).sum::<usize>()
                })
                .sum();
            assert!(total_lines <= MAX_DISPLAY_LINES);
            assert!(total_bytes <= MAX_DISPLAY_BYTES);
            assert!(
                nodes
                    .iter()
                    .all(|node| node.lines.iter().all(|line| !line.contains('\n')))
            );
            assert!(
                result
                    .tools
                    .iter()
                    .all(|tool| tool.output.as_deref() == Some(body.as_str()))
            );
        }
    }

    #[test]
    fn session_details_812_tools_only_charge_visible_bodies_and_keep_selected_text() {
        let first_arguments = format!("FIRST_ARGUMENTS:{}", "a".repeat(471));
        let first_output = format!("FIRST_OUTPUT:{}:OUTPUT_END", "o".repeat(40_759));
        assert_eq!(first_arguments.len(), 487);
        assert_eq!(first_output.len(), 40_783);
        let mut result = SessionDetails {
            tools: (0..812)
                .map(|index| DetailTool {
                    call_id: format!("call-{index}"),
                    name: "exec".into(),
                    arguments: Some(format!("arguments-{index}")),
                    output: Some(format!("output-{index}")),
                    exit_code: Some(0),
                    duration_ms: Some(1),
                    timestamp: None,
                    turn_id: Some("one".into()),
                    test_command: false,
                })
                .collect(),
            metadata: BTreeMap::from([(
                "Sandbox policy".into(),
                "HIDDEN_CONFIGURATION".repeat(3000),
            )]),
            file_changes: vec![DetailEvidence {
                text: "HIDDEN_DIFF".repeat(5900),
                timestamp: None,
                turn_id: Some("one".into()),
            }],
            ..SessionDetails::default()
        };
        result.tools[0].arguments = Some(first_arguments.clone());
        result.tools[0].output = Some(first_output.clone());
        result.tools[1].output = Some("HIDDEN_OTHER_OUTPUT".repeat(3600));
        let collapsed = result.detail_sections(&HashSet::new());
        assert_eq!(collapsed[5].title, "Tool calls (812)");
        assert!(
            collapsed
                .iter()
                .all(|section| section.lines.is_empty() && section.children.is_empty())
        );

        let mut expanded = HashSet::from(["tools".into()]);
        let listed = result.detail_sections(&expanded);
        assert_eq!(listed[5].children.len(), 812);
        assert!(
            listed[5]
                .children
                .iter()
                .all(|child| child.lines.is_empty() && child.children.is_empty())
        );
        let identities: Vec<_> = listed[5]
            .children
            .iter()
            .map(|child| child.id.clone())
            .collect();
        for index in [0, 811] {
            expanded = HashSet::from([
                "tools".into(),
                identities[index].clone(),
                format!("{}/arguments", identities[index]),
                format!("{}/output", identities[index]),
            ]);
            let sections = result.detail_sections(&expanded);
            assert_eq!(sections[5].children.len(), 812);
            assert_eq!(
                sections[5]
                    .children
                    .iter()
                    .map(|child| child.id.clone())
                    .collect::<Vec<_>>(),
                identities
            );
            let selected = &sections[5].children[index];
            assert_eq!(
                selected.children[0].lines,
                [result.tools[index].arguments.as_deref().unwrap()]
            );
            assert_eq!(
                selected.children[1].lines,
                [result.tools[index].output.as_deref().unwrap()]
            );
            let mut nodes = Vec::new();
            section_nodes(&sections, &mut nodes);
            assert!(
                nodes
                    .iter()
                    .filter(|node| expanded.contains(&node.id))
                    .all(|node| !node.children.is_empty()
                        || node.lines.iter().any(|line| !line.trim().is_empty()))
            );
            assert!(nodes.iter().all(|node| node.lines.iter().all(|line| {
                !line.contains("HIDDEN_DIFF")
                    && !line.contains("HIDDEN_OTHER_OUTPUT")
                    && !line.contains("HIDDEN_CONFIGURATION")
            })));
            assert!(nodes.iter().all(|node| node.id != SECTION_TRUNCATION_ID));
        }
        assert_eq!(
            result.tools[0].arguments.as_deref(),
            Some(first_arguments.as_str())
        );
        assert_eq!(
            result.tools[0].output.as_deref(),
            Some(first_output.as_str())
        );
    }

    #[test]
    fn session_details_expanded_bodies_distinguish_empty_missing_and_retention_limits() {
        let mut parser = parser(None);
        parser.record(&record(
            "response_item",
            json!({"type":"function_call","call_id":"missing","turn_id":"one","name":"exec"}),
        ));
        parser.record(&record("response_item", json!({"type":"function_call","call_id":"empty","turn_id":"one","name":"exec","arguments":""})));
        parser.record(&record(
            "response_item",
            json!({"type":"function_call_output","call_id":"empty","turn_id":"one","output":""}),
        ));
        parser.record(&record("response_item", json!({"type":"function_call","call_id":"partial","turn_id":"one","name":"exec","arguments":"p".repeat(MAX_TEXT_BYTES + 1)})));
        parser.content_bytes = MAX_CONTENT_BYTES - 1;
        parser.record(&record("response_item", json!({"type":"function_call","call_id":"omitted","turn_id":"one","name":"exec","arguments":"界"})));
        // Preserve short exit/duration metadata before dropping a full output.
        parser.content_bytes = MAX_CONTENT_BYTES;
        parser.record(&record("response_item", json!({"type":"function_call_output","call_id":"omitted","turn_id":"one","output":"Wall time: 1.25 seconds\nProcess exited with code 7\nOutput:\nrecorded stdout"})));
        let result = parser.finish();
        let tools = &expanded_sections(&result)[5];
        assert!(
            tools.children[0].children[0]
                .lines
                .iter()
                .any(|line| line.contains("unrecorded"))
        );
        assert_eq!(
            tools.children[1].children[0].lines,
            ["Recorded content is empty."]
        );
        assert_eq!(
            tools.children[1].children[1].lines,
            ["Recorded content is empty."]
        );
        assert!(tools.children[2].children[0].lines[0].contains("retained content truncated"));
        assert!(tools.children[3].children.iter().all(|body| {
            body.lines
                .iter()
                .any(|line| line.contains("content not retained"))
        }));
        assert!(
            tools.children[3]
                .title
                .contains("Exit: 7 | Duration: 1250 ms")
        );
        assert!(result.tools[3].arguments.is_none());
        assert!(result.tools[3].output.is_none());
        assert_eq!(
            result.tool_retention["omitted"].arguments,
            DetailRetention::Omitted
        );
        assert_eq!(
            result.tool_retention["omitted"].output,
            DetailRetention::Omitted
        );
    }

    #[test]
    fn session_details_display_limit_keeps_expanded_leaves_explained() {
        let result = SessionDetails {
            tools: vec![DetailTool {
                call_id: "both-leaves".into(),
                name: "exec".into(),
                arguments: Some("argument line\n".repeat(MAX_TEXT_BYTES)),
                output: Some("real output".into()),
                exit_code: None,
                duration_ms: None,
                timestamp: None,
                turn_id: None,
                test_command: false,
            }],
            file_changes: vec![DetailEvidence {
                text: "recorded diff".into(),
                timestamp: None,
                turn_id: None,
            }],
            ..SessionDetails::default()
        };
        let listed = result.detail_sections(&HashSet::from(["tools".into()]));
        let call_id = listed[5].children[0].id.clone();
        let expanded = HashSet::from([
            "tools".into(),
            "files".into(),
            "source".into(),
            call_id.clone(),
            format!("{call_id}/arguments"),
            format!("{call_id}/output"),
        ]);
        let sections = result.detail_sections(&expanded);
        let output = &sections[5].children[0].children[1];
        assert_eq!(output.lines, [SECTION_BODY_OMITTED]);
        assert_eq!(sections[6].lines, [SECTION_BODY_OMITTED]);
        let mut nodes = Vec::new();
        section_nodes(&sections, &mut nodes);
        assert!(
            nodes
                .iter()
                .filter(|node| expanded.contains(&node.id))
                .all(|node| !node.children.is_empty()
                    || node.lines.iter().any(|line| !line.trim().is_empty()))
        );
        assert!(nodes.iter().map(|node| 1 + node.lines.len()).sum::<usize>() <= MAX_DISPLAY_LINES);
        assert!(
            nodes
                .iter()
                .map(|node| node.id.len()
                    + node.title.len()
                    + 2
                    + node.lines.iter().map(|line| line.len() + 1).sum::<usize>())
                .sum::<usize>()
                <= MAX_DISPLAY_BYTES
        );
        let mut leading_blank = result;
        leading_blank.tools[0].arguments =
            Some(format!("{}visible tail", "\n".repeat(MAX_TEXT_BYTES)));
        let sections = leading_blank.detail_sections(&expanded);
        assert_eq!(
            sections[5].children[0].children[0].lines,
            [SECTION_BODY_OMITTED]
        );
        assert_eq!(
            sections[5].children[0].children[1].lines,
            [SECTION_BODY_OMITTED]
        );
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
