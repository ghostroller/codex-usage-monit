use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::config::CollectConfig;
use crate::domain::{
    Confidence, LimitWindow, Provenance, RateObservation, RolloutDataset, TaskRecord, TaskStatus,
    TokenUsage, TurnRecord, TurnStatus, UsageCall,
};
use crate::session_index::load_thread_titles;

const TURN_MESSAGE_PREVIEW_CHARS: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RolloutFile {
    path: PathBuf,
    modified_at: DateTime<Utc>,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl FileFingerprint {
    fn from_metadata(metadata: &Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedFile {
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug)]
enum ParsedEvent {
    SessionMeta {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    ForeignCounterBaseline(TokenUsage),
    UserMessage {
        preview: String,
        turn_id: Option<String>,
    },
    ThreadSettingsApplied {
        service_tier: String,
    },
    TurnContext {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    EventMessage {
        timestamp: DateTime<Utc>,
        line_number: usize,
        payload: Map<String, Value>,
    },
}

#[derive(Clone, Debug, Default)]
struct ParsedFile {
    owner_thread_id: Option<String>,
    activity_updated_at: Option<DateTime<Utc>>,
    events: Vec<ParsedEvent>,
    parsed_lines: usize,
    skipped_lines: usize,
    unreadable_files: usize,
    warnings: Vec<String>,
    complete: bool,
}

#[derive(Clone, Debug)]
struct CachedFile {
    fingerprint: FileFingerprint,
    parsed: ParsedFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CacheKey {
    codex_home: PathBuf,
    redact_content: bool,
}

#[derive(Clone, Debug, Default)]
struct ReducedRollouts {
    threads: HashMap<String, ThreadBuilder>,
    dataset: RolloutDataset,
}

/// Diagnostics for the most recent cached scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RolloutCacheRefresh {
    pub reused_files: usize,
    pub reparsed_files: usize,
    pub rebuilt: bool,
    /// Number of `session_index.jsonl` content-read attempts.
    pub session_index_reads: usize,
    /// Whether the cached session-title snapshot was reused unchanged.
    pub session_index_reused: bool,
}

/// Reuses parsed rollout files across refreshes while preserving global token
/// counter semantics during reduction.
#[derive(Debug, Default)]
pub struct RolloutCache {
    key: Option<CacheKey>,
    files: HashMap<PathBuf, CachedFile>,
    selected: Vec<SelectedFile>,
    reduced: Option<ReducedRollouts>,
    session_titles: SessionTitleCache,
    last_refresh: RolloutCacheRefresh,
}

#[derive(Debug, Default)]
struct SessionTitleCache {
    state: SessionTitleState,
    titles: HashMap<String, String>,
}

#[derive(Debug, Default)]
enum SessionTitleState {
    #[default]
    Unknown,
    Missing,
    Loaded(FileFingerprint),
}

#[derive(Clone, Debug, Default)]
struct ThreadBuilder {
    thread_id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    source: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    service_tier: Option<String>,
    active_turn_ids: Vec<String>,
    last_turn_id: Option<String>,
    previous_cumulative: Option<TokenUsage>,
    token_usage: TokenUsage,
    turns: HashMap<String, TurnBuilder>,
}

#[derive(Clone, Debug, Default)]
struct TurnBuilder {
    turn_id: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    message_preview: Option<String>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    duration_ms: Option<u64>,
    status: TurnStatus,
    token_usage: TokenUsage,
}

/// Scans recent Codex rollout files without reading authentication material.
///
/// Rollout formats are internal to Codex, so this adapter deliberately parses
/// only the fields it understands and ignores unknown records and fields.
pub fn scan_rollouts(config: &CollectConfig, now: DateTime<Utc>) -> Result<RolloutDataset> {
    RolloutCache::default().scan(config, now)
}

impl RolloutCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_refresh(&self) -> RolloutCacheRefresh {
        self.last_refresh
    }

    /// Scans recent rollouts, reparsing rollout files and reloading the session
    /// title index only when their metadata fingerprints change.
    pub fn scan(&mut self, config: &CollectConfig, now: DateTime<Utc>) -> Result<RolloutDataset> {
        let key = CacheKey {
            codex_home: config.codex_home.clone(),
            redact_content: config.redact_content,
        };
        if self.key.as_ref() != Some(&key) {
            self.files.clear();
            self.selected.clear();
            self.reduced = None;
            self.session_titles = SessionTitleCache::default();
            self.key = Some(key);
        }

        let mut discovery = RolloutDataset::default();
        let mut files = discover_rollout_files(config, now, &mut discovery);

        // Parsing older files first preserves cumulative counter order when a
        // thread happens to span more than one rollout file.
        files.sort_by(|left, right| {
            left.modified_at
                .cmp(&right.modified_at)
                .then_with(|| left.path.cmp(&right.path))
        });

        let mut refresh = RolloutCacheRefresh::default();
        self.refresh_session_titles(config, &mut discovery, &mut refresh);
        for file in &files {
            let reusable = self.files.get(&file.path).is_some_and(|cached| {
                cached.fingerprint == file.fingerprint && cached.parsed.complete
            });
            if reusable {
                refresh.reused_files += 1;
                continue;
            }

            let cached = parse_stable_rollout_file(file, config);
            self.files.insert(file.path.clone(), cached);
            refresh.reparsed_files += 1;
        }

        let selected = files
            .iter()
            .filter_map(|file| {
                self.files.get(&file.path).map(|cached| SelectedFile {
                    path: file.path.clone(),
                    fingerprint: cached.fingerprint.clone(),
                })
            })
            .collect::<Vec<_>>();
        let selected_changed = selected != self.selected;
        let must_rebuild = self.reduced.is_none() || selected_changed || refresh.reparsed_files > 0;

        if must_rebuild {
            self.reduced = Some(reduce_cached_files(&files, &self.files, config));
            refresh.rebuilt = true;
        }
        self.selected = selected;

        let selected_paths = files
            .iter()
            .map(|file| file.path.as_path())
            .collect::<std::collections::HashSet<_>>();
        self.files
            .retain(|path, _| selected_paths.contains(path.as_path()));
        self.last_refresh = refresh;

        Ok(materialize_dataset(
            self.reduced
                .as_ref()
                .expect("a scan always initializes reduced rollout state"),
            discovery,
            config,
            now,
            &self.session_titles.titles,
        ))
    }

    fn refresh_session_titles(
        &mut self,
        config: &CollectConfig,
        discovery: &mut RolloutDataset,
        refresh: &mut RolloutCacheRefresh,
    ) {
        if config.redact_content {
            self.session_titles = SessionTitleCache::default();
            return;
        }

        let path = config.codex_home.join("session_index.jsonl");
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if matches!(self.session_titles.state, SessionTitleState::Missing) {
                    refresh.session_index_reused = true;
                } else {
                    self.session_titles.state = SessionTitleState::Missing;
                    self.session_titles.titles.clear();
                }
                return;
            }
            Err(error) => {
                self.session_titles = SessionTitleCache::default();
                discovery.warnings.push(format!(
                    "session title index unavailable: could not inspect {}: {error}",
                    path.display()
                ));
                return;
            }
        };
        let fingerprint = match FileFingerprint::from_metadata(&metadata) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                self.session_titles = SessionTitleCache::default();
                discovery.warnings.push(format!(
                    "session title index unavailable: could not fingerprint {}: {error}",
                    path.display()
                ));
                return;
            }
        };
        if matches!(
            &self.session_titles.state,
            SessionTitleState::Loaded(cached) if cached == &fingerprint
        ) {
            refresh.session_index_reused = true;
            return;
        }

        refresh.session_index_reads += 1;
        match load_thread_titles(&config.codex_home) {
            Ok(titles) => {
                self.session_titles.state = SessionTitleState::Loaded(fingerprint);
                self.session_titles.titles = titles;
            }
            Err(error) => {
                self.session_titles = SessionTitleCache::default();
                discovery
                    .warnings
                    .push(format!("session title index unavailable: {error:#}"));
            }
        }
    }
}

fn reduce_cached_files(
    files: &[RolloutFile],
    cache: &HashMap<PathBuf, CachedFile>,
    config: &CollectConfig,
) -> ReducedRollouts {
    let mut reduced = ReducedRollouts::default();
    for file in files {
        reduced.dataset.stats.scanned_files += 1;
        let Some(cached) = cache.get(&file.path) else {
            continue;
        };
        let parsed = &cached.parsed;
        reduced.dataset.stats.parsed_lines += parsed.parsed_lines;
        reduced.dataset.stats.skipped_lines += parsed.skipped_lines;
        reduced.dataset.stats.unreadable_files += parsed.unreadable_files;
        reduced.dataset.warnings.extend(parsed.warnings.clone());
        replay_rollout_file(
            &file.path,
            parsed,
            config,
            &mut reduced.threads,
            &mut reduced.dataset,
        );
    }
    reduced
}

fn materialize_dataset(
    reduced: &ReducedRollouts,
    mut discovery: RolloutDataset,
    config: &CollectConfig,
    now: DateTime<Utc>,
    thread_titles: &HashMap<String, String>,
) -> RolloutDataset {
    discovery.stats.scanned_files = reduced.dataset.stats.scanned_files;
    discovery.stats.parsed_lines = reduced.dataset.stats.parsed_lines;
    discovery.stats.skipped_lines = reduced.dataset.stats.skipped_lines;
    discovery.stats.ambiguous_token_resets = reduced.dataset.stats.ambiguous_token_resets;
    discovery.stats.unreadable_files += reduced.dataset.stats.unreadable_files;
    discovery.warnings.extend(reduced.dataset.warnings.clone());
    discovery.calls = reduced.dataset.calls.clone();
    discovery.rate_observations = reduced.dataset.rate_observations.clone();
    finish_dataset(
        config,
        now,
        reduced.threads.clone(),
        thread_titles,
        &mut discovery,
    );
    discovery
}

fn discover_rollout_files(
    config: &CollectConfig,
    now: DateTime<Utc>,
    dataset: &mut RolloutDataset,
) -> Vec<RolloutFile> {
    let lookback_days = config.lookback_days.max(0);
    let cutoff = ChronoDuration::try_days(lookback_days)
        .and_then(|lookback| now.checked_sub_signed(lookback))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let roots = [
        config.codex_home.join("sessions"),
        config.codex_home.join("archived_sessions"),
    ];
    let mut files = Vec::new();

    if !roots.iter().any(|root| root.is_dir()) {
        dataset.stats.unreadable_files += 1;
        dataset.warnings.push(format!(
            "no Codex rollout directories found under {}",
            config.codex_home.display()
        ));
        return files;
    }

    for root in roots {
        if !root.is_dir() {
            continue;
        }

        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    dataset.stats.unreadable_files += 1;
                    dataset.warnings.push(format!(
                        "could not inspect rollout path under {}: {error}",
                        root.display()
                    ));
                    continue;
                }
            };

            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|v| v.to_str()) != Some("jsonl")
            {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    dataset.stats.unreadable_files += 1;
                    dataset.warnings.push(format!(
                        "could not read metadata for {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            };
            let modified_at = match metadata.modified() {
                Ok(modified_at) => DateTime::<Utc>::from(modified_at),
                Err(error) => {
                    dataset.stats.unreadable_files += 1;
                    dataset.warnings.push(format!(
                        "could not read modification time for {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            };
            let fingerprint = match FileFingerprint::from_metadata(&metadata) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    dataset.stats.unreadable_files += 1;
                    dataset.warnings.push(format!(
                        "could not fingerprint {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            };

            if modified_at >= cutoff {
                dataset.stats.discovered_files += 1;
                files.push(RolloutFile {
                    path: entry.into_path(),
                    modified_at,
                    fingerprint,
                });
            }
        }
    }

    files.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| left.path.cmp(&right.path))
    });
    dataset.stats.truncated_files = files.len().saturating_sub(config.max_files);
    files.truncate(config.max_files);
    files
}

fn parse_stable_rollout_file(file: &RolloutFile, config: &CollectConfig) -> CachedFile {
    let mut candidate = file.clone();
    for attempt in 0..2 {
        let mut parsed = parse_rollout_file(&candidate, config);
        let after = match inspect_rollout_file(&candidate.path) {
            Ok(after) => after,
            Err(error) => {
                parsed.complete = false;
                parsed.unreadable_files += 1;
                parsed.warnings.push(format!(
                    "could not inspect {} after reading: {error}",
                    candidate.path.display()
                ));
                return CachedFile {
                    fingerprint: candidate.fingerprint,
                    parsed,
                };
            }
        };

        if after.fingerprint == candidate.fingerprint {
            return CachedFile {
                fingerprint: after.fingerprint,
                parsed,
            };
        }
        if attempt == 0 {
            candidate = after;
            continue;
        }

        parsed.complete = false;
        parsed.unreadable_files += 1;
        parsed.warnings.push(format!(
            "{} changed repeatedly while being read; the snapshot contains a stable parsed prefix",
            candidate.path.display()
        ));
        return CachedFile {
            fingerprint: after.fingerprint,
            parsed,
        };
    }
    unreachable!("stable rollout parsing attempts are bounded")
}

fn inspect_rollout_file(path: &Path) -> std::io::Result<RolloutFile> {
    let metadata = path.metadata()?;
    let fingerprint = FileFingerprint::from_metadata(&metadata)?;
    Ok(RolloutFile {
        path: path.to_owned(),
        modified_at: DateTime::<Utc>::from(fingerprint.modified),
        fingerprint,
    })
}

fn parse_rollout_file(file: &RolloutFile, config: &CollectConfig) -> ParsedFile {
    let mut parsed = ParsedFile::default();
    let handle = match File::open(&file.path) {
        Ok(handle) => handle,
        Err(error) => {
            parsed.unreadable_files += 1;
            parsed
                .warnings
                .push(format!("could not open {}: {error}", file.path.display()));
            return parsed;
        }
    };

    let mut owning_thread_id: Option<String> = None;
    let mut owning_created_at: Option<DateTime<Utc>> = None;
    let mut replaying_foreign_history = false;
    for (line_index, line) in BufReader::new(handle).lines().enumerate() {
        let line_number = line_index + 1;
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                parsed.skipped_lines += 1;
                parsed.warnings.push(format!(
                    "could not read {} line {line_number}: {error}",
                    file.path.display()
                ));
                continue;
            }
        };

        let record: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(record)) => Value::Object(record),
            Ok(_) => {
                parsed.skipped_lines += 1;
                parsed.warnings.push(format!(
                    "ignored non-object JSON at {} line {line_number}",
                    file.path.display()
                ));
                continue;
            }
            Err(error) => {
                parsed.skipped_lines += 1;
                parsed.warnings.push(format!(
                    "ignored malformed JSON at {} line {line_number}: {error}",
                    file.path.display()
                ));
                continue;
            }
        };
        parsed.parsed_lines += 1;

        let record_type = string_field(&record, &["type"]);
        let timestamp = value_at(&record, &["timestamp"])
            .and_then(parse_timestamp)
            .unwrap_or(file.modified_at);

        if record_type == Some("session_meta") {
            let Some(payload) = object_at(&record, &["payload"]) else {
                continue;
            };
            let Some(thread_id) = string_field_in(payload, &["id"]).map(str::to_owned) else {
                parsed.warnings.push(format!(
                    "session metadata without an id at {} line {line_number}",
                    file.path.display()
                ));
                continue;
            };

            match owning_thread_id.as_deref() {
                None => {
                    owning_created_at = payload
                        .get("timestamp")
                        .and_then(parse_timestamp)
                        .or(Some(timestamp));
                    owning_thread_id = Some(thread_id.clone());
                    parsed.owner_thread_id = Some(thread_id);
                    replaying_foreign_history = is_forked_session(payload);
                    parsed.events.push(ParsedEvent::SessionMeta {
                        timestamp,
                        payload: projected_payload(
                            payload,
                            &[
                                "cwd",
                                "source",
                                "thread_source",
                                "threadSource",
                                "originator",
                                "timestamp",
                            ],
                        ),
                    });
                }
                Some(owner) if owner == thread_id => {
                    replaying_foreign_history = false;
                    parsed.events.push(ParsedEvent::SessionMeta {
                        timestamp,
                        payload: projected_payload(
                            payload,
                            &[
                                "cwd",
                                "source",
                                "thread_source",
                                "threadSource",
                                "originator",
                                "timestamp",
                            ],
                        ),
                    });
                }
                Some(_) => {
                    // Forked/subagent rollouts can embed a complete parent
                    // history after their own metadata. It is context, not
                    // activity owned by this file's thread.
                    replaying_foreign_history = true;
                }
            }
            continue;
        }

        let Some(thread_id) = owning_thread_id.as_deref() else {
            // Valid unknown/prelude records are tolerated. A standard rollout
            // always starts with session_meta, so there is no safe attribution.
            continue;
        };

        if replaying_foreign_history {
            if starts_owning_segment(
                record_type,
                object_at(&record, &["payload"]),
                thread_id,
                owning_created_at,
            ) {
                replaying_foreign_history = false;
            } else {
                // The child counter normally continues from the embedded
                // parent's cumulative total. Retain only that baseline so the
                // first child call is an exact delta; do not emit parent calls,
                // turns, titles, or rate observations.
                if record_type == Some("event_msg")
                    && let Some(payload) = object_at(&record, &["payload"])
                    && string_field_in(payload, &["type"]) == Some("token_count")
                    && let Some(total_usage) = total_token_usage(payload)
                {
                    parsed
                        .events
                        .push(ParsedEvent::ForeignCounterBaseline(total_usage));
                }
                continue;
            }
        }
        set_max_timestamp(&mut parsed.activity_updated_at, timestamp);

        match record_type {
            Some("turn_context") => {
                if let Some(payload) = object_at(&record, &["payload"]) {
                    parsed.events.push(ParsedEvent::TurnContext {
                        timestamp,
                        payload: projected_payload(
                            payload,
                            &[
                                "turn_id",
                                "turnId",
                                "model",
                                "effort",
                                "reasoning_effort",
                                "reasoningEffort",
                            ],
                        ),
                    });
                }
            }
            Some("event_msg") => {
                if let Some(payload) = object_at(&record, &["payload"]) {
                    if string_field_in(payload, &["type"]) == Some("user_message") {
                        if !config.redact_content
                            && let Some(title) = payload
                                .get("message")
                                .and_then(Value::as_str)
                                .and_then(title_preview)
                        {
                            parsed.events.push(ParsedEvent::UserMessage {
                                preview: title,
                                turn_id: string_field_in(payload, &["turn_id", "turnId"])
                                    .map(str::to_owned),
                            });
                        }
                    } else if string_field_in(payload, &["type"]) == Some("thread_settings_applied")
                    {
                        if let Some(service_tier) = payload
                            .get("thread_settings")
                            .or_else(|| payload.get("threadSettings"))
                            .and_then(Value::as_object)
                            .and_then(|settings| {
                                string_field_in(settings, &["service_tier", "serviceTier"])
                            })
                        {
                            parsed.events.push(ParsedEvent::ThreadSettingsApplied {
                                service_tier: service_tier.to_owned(),
                            });
                        }
                    } else if should_cache_event_message(payload) {
                        parsed.events.push(ParsedEvent::EventMessage {
                            timestamp,
                            line_number,
                            payload: projected_payload(
                                payload,
                                &[
                                    "type",
                                    "turn_id",
                                    "turnId",
                                    "started_at",
                                    "startedAt",
                                    "completed_at",
                                    "completedAt",
                                    "duration_ms",
                                    "durationMs",
                                    "reason",
                                    "info",
                                    "total_token_usage",
                                    "totalTokenUsage",
                                    "rate_limits",
                                    "rateLimits",
                                ],
                            ),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    parsed.complete = true;
    parsed
}

fn is_forked_session(payload: &Map<String, Value>) -> bool {
    ["parent_thread_id", "forked_from_id"]
        .into_iter()
        .any(|field| {
            payload
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        || payload
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|source| source.contains_key("subagent"))
}

fn should_cache_event_message(payload: &Map<String, Value>) -> bool {
    matches!(
        string_field_in(payload, &["type"]),
        Some("task_started" | "task_complete" | "turn_aborted" | "token_count")
    )
}

fn projected_payload(payload: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|field| {
            payload
                .get(*field)
                .cloned()
                .map(|value| ((*field).to_owned(), value))
        })
        .collect()
}

fn replay_rollout_file(
    path: &Path,
    parsed: &ParsedFile,
    config: &CollectConfig,
    threads: &mut HashMap<String, ThreadBuilder>,
    dataset: &mut RolloutDataset,
) {
    let Some(thread_id) = parsed.owner_thread_id.as_deref() else {
        return;
    };
    let thread = threads
        .entry(thread_id.to_owned())
        .or_insert_with(|| ThreadBuilder {
            thread_id: thread_id.to_owned(),
            ..ThreadBuilder::default()
        });

    for event in &parsed.events {
        match event {
            ParsedEvent::SessionMeta { timestamp, payload } => {
                apply_session_meta(thread, payload, *timestamp);
            }
            ParsedEvent::ForeignCounterBaseline(total_usage) => {
                thread.previous_cumulative = Some(*total_usage);
            }
            ParsedEvent::UserMessage { preview, turn_id } => {
                apply_user_message(thread, preview, turn_id.as_deref());
            }
            ParsedEvent::ThreadSettingsApplied { service_tier } => {
                thread.service_tier = Some(service_tier.clone());
            }
            ParsedEvent::TurnContext { timestamp, payload } => {
                apply_turn_context(thread, payload, *timestamp);
            }
            ParsedEvent::EventMessage {
                timestamp,
                line_number,
                payload,
            } => {
                apply_event_msg(
                    thread,
                    payload,
                    *timestamp,
                    config,
                    path,
                    *line_number,
                    dataset,
                );
            }
        }
    }
    if let Some(updated_at) = parsed.activity_updated_at {
        set_max_timestamp(&mut thread.updated_at, updated_at);
    }
}

fn apply_session_meta(
    thread: &mut ThreadBuilder,
    payload: &Map<String, Value>,
    timestamp: DateTime<Utc>,
) {
    if thread.cwd.is_none() {
        thread.cwd = string_field_in(payload, &["cwd"]).map(PathBuf::from);
    }
    if thread.source.is_none() {
        thread.source = session_source_label(payload);
    }

    let created_at = payload
        .get("timestamp")
        .and_then(parse_timestamp)
        .unwrap_or(timestamp);
    set_min_timestamp(&mut thread.created_at, created_at);
    set_max_timestamp(&mut thread.updated_at, timestamp);
}

fn apply_user_message(thread: &mut ThreadBuilder, message: &str, explicit_turn_id: Option<&str>) {
    if thread.title.is_none() {
        thread.title = Some(message.to_owned());
    }

    let preview = shorten_preview(message, TURN_MESSAGE_PREVIEW_CHARS);
    if let Some(turn_id) = explicit_turn_id {
        let turn = ensure_turn(thread, turn_id);
        if turn.message_preview.is_none() {
            turn.message_preview = Some(preview);
        }
    } else if let Some(turn_id) = thread.active_turn_ids.last().cloned() {
        let turn = ensure_turn(thread, &turn_id);
        if turn.message_preview.is_none() {
            turn.message_preview = Some(preview);
        }
    }
}

fn apply_turn_context(
    thread: &mut ThreadBuilder,
    payload: &Map<String, Value>,
    timestamp: DateTime<Utc>,
) {
    let Some(turn_id) = string_field_in(payload, &["turn_id", "turnId"]).map(str::to_owned) else {
        return;
    };

    if !thread
        .active_turn_ids
        .iter()
        .any(|active| active == &turn_id)
    {
        activate_turn(thread, &turn_id, timestamp);
    }
    let turn = ensure_turn(thread, &turn_id);
    if let Some(model) = string_field_in(payload, &["model"]) {
        turn.model = Some(model.to_owned());
    }
    if let Some(reasoning_effort) =
        string_field_in(payload, &["effort", "reasoning_effort", "reasoningEffort"])
    {
        turn.reasoning_effort = Some(reasoning_effort.to_owned());
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_event_msg(
    thread: &mut ThreadBuilder,
    payload: &Map<String, Value>,
    timestamp: DateTime<Utc>,
    config: &CollectConfig,
    path: &Path,
    line_number: usize,
    dataset: &mut RolloutDataset,
) {
    match string_field_in(payload, &["type"]) {
        Some("user_message") => {
            if thread.title.is_none() && !config.redact_content {
                thread.title = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .and_then(title_preview);
            }
        }
        Some("task_started") => {
            let Some(turn_id) = string_field_in(payload, &["turn_id", "turnId"]).map(str::to_owned)
            else {
                return;
            };
            let started_at = payload
                .get("started_at")
                .or_else(|| payload.get("startedAt"))
                .and_then(parse_timestamp)
                .unwrap_or(timestamp);
            activate_turn(thread, &turn_id, started_at);
        }
        Some("task_complete") => {
            let Some(turn_id) = string_field_in(payload, &["turn_id", "turnId"]).map(str::to_owned)
            else {
                return;
            };
            let completed_at = payload
                .get("completed_at")
                .or_else(|| payload.get("completedAt"))
                .and_then(parse_timestamp)
                .unwrap_or(timestamp);
            finish_turn(
                thread,
                &turn_id,
                payload,
                completed_at,
                TurnStatus::Completed,
            );
        }
        Some("turn_aborted") => {
            let Some(turn_id) = string_field_in(payload, &["turn_id", "turnId"]).map(str::to_owned)
            else {
                return;
            };
            let completed_at = payload
                .get("completed_at")
                .or_else(|| payload.get("completedAt"))
                .and_then(parse_timestamp)
                .unwrap_or(timestamp);
            let reason = string_field_in(payload, &["reason"]).unwrap_or_default();
            let status = if reason.contains("fail") || reason.contains("error") {
                TurnStatus::Failed
            } else {
                TurnStatus::Interrupted
            };
            finish_turn(thread, &turn_id, payload, completed_at, status);
        }
        Some("token_count") => {
            apply_token_count(thread, payload, timestamp, path, line_number, dataset);
        }
        _ => {}
    }
}

fn activate_turn(thread: &mut ThreadBuilder, turn_id: &str, started_at: DateTime<Utc>) {
    let service_tier = thread.service_tier.clone();
    let turn = ensure_turn(thread, turn_id);
    if matches!(
        turn.status,
        TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed
    ) {
        return;
    }
    if turn.service_tier.is_none() {
        turn.service_tier = service_tier;
    }
    set_min_timestamp(&mut turn.started_at, started_at);
    if matches!(turn.status, TurnStatus::Unknown | TurnStatus::Stale) {
        turn.status = TurnStatus::InProgress;
    }
    thread.active_turn_ids.retain(|active| active != turn_id);
    thread.active_turn_ids.push(turn_id.to_owned());
    thread.last_turn_id = None;
}

fn finish_turn(
    thread: &mut ThreadBuilder,
    turn_id: &str,
    payload: &Map<String, Value>,
    completed_at: DateTime<Utc>,
    status: TurnStatus,
) {
    let turn = ensure_turn(thread, turn_id);
    turn.completed_at = Some(completed_at);
    turn.status = status;
    turn.duration_ms = u64_field_in(payload, &["duration_ms", "durationMs"]).or_else(|| {
        turn.started_at.and_then(|started_at| {
            completed_at
                .signed_duration_since(started_at)
                .num_milliseconds()
                .try_into()
                .ok()
        })
    });
    thread.active_turn_ids.retain(|active| active != turn_id);
    thread.last_turn_id = Some(turn_id.to_owned());
}

fn apply_token_count(
    thread: &mut ThreadBuilder,
    payload: &Map<String, Value>,
    timestamp: DateTime<Utc>,
    path: &Path,
    line_number: usize,
    dataset: &mut RolloutDataset,
) {
    if let Some(rate_limits) = payload
        .get("rate_limits")
        .or_else(|| payload.get("rateLimits"))
        .and_then(Value::as_object)
        && let Some(observation) = parse_rate_observation(
            rate_limits,
            timestamp,
            &thread.thread_id,
            thread
                .active_turn_ids
                .last()
                .map(String::as_str)
                .or(thread.last_turn_id.as_deref()),
        )
    {
        dataset.rate_observations.push(observation);
    }

    let total_usage = total_token_usage(payload);
    let Some(total_usage) = total_usage else {
        return;
    };

    let delta = match thread.previous_cumulative {
        None => total_usage,
        Some(previous) if previous == total_usage => return,
        Some(previous) => match total_usage.delta_from(previous) {
            Some(delta) => delta,
            None => {
                dataset.warnings.push(format!(
                    "token counter reset for thread {} at {} line {line_number}; re-established the cumulative baseline without counting the ambiguous reset sample",
                    thread.thread_id,
                    path.display()
                ));
                dataset.stats.ambiguous_token_resets += 1;
                thread.previous_cumulative = Some(total_usage);
                return;
            }
        },
    };
    thread.previous_cumulative = Some(total_usage);
    if delta.is_zero() {
        return;
    }

    thread.token_usage.add_assign(delta);
    let turn_id = thread
        .active_turn_ids
        .last()
        .cloned()
        .or_else(|| thread.last_turn_id.clone());
    let model = turn_id
        .as_deref()
        .and_then(|turn_id| thread.turns.get(turn_id))
        .and_then(|turn| turn.model.clone());
    if let Some(turn_id) = turn_id.as_deref() {
        ensure_turn(thread, turn_id).token_usage.add_assign(delta);
    }
    dataset.calls.push(UsageCall {
        timestamp,
        thread_id: thread.thread_id.clone(),
        turn_id,
        model,
        tokens: delta,
    });
}

fn total_token_usage(payload: &Map<String, Value>) -> Option<TokenUsage> {
    payload
        .get("info")
        .and_then(Value::as_object)
        .and_then(|info| {
            info.get("total_token_usage")
                .or_else(|| info.get("totalTokenUsage"))
        })
        .or_else(|| payload.get("total_token_usage"))
        .or_else(|| payload.get("totalTokenUsage"))
        .and_then(parse_token_usage)
}

fn starts_owning_segment(
    record_type: Option<&str>,
    payload: Option<&Map<String, Value>>,
    owning_thread_id: &str,
    owning_created_at: Option<DateTime<Utc>>,
) -> bool {
    if record_type != Some("event_msg") {
        return false;
    }
    let Some(payload) = payload else {
        return false;
    };
    if string_field_in(payload, &["type"]) != Some("task_started") {
        return false;
    }

    let turn_id = string_field_in(payload, &["turn_id", "turnId"]);
    let uuid_time_is_owning = turn_id
        .and_then(uuid_v7_timestamp_ms)
        .zip(uuid_v7_timestamp_ms(owning_thread_id))
        .is_some_and(|(turn_timestamp, owner_timestamp)| turn_timestamp >= owner_timestamp);

    let started_at = payload
        .get("started_at")
        .or_else(|| payload.get("startedAt"))
        .and_then(parse_timestamp);
    let timestamp_is_owning = match (started_at, owning_created_at) {
        // started_at is second-granularity in current rollouts, hence tolerance.
        (Some(started_at), Some(created_at)) => {
            started_at >= created_at - ChronoDuration::seconds(2)
        }
        _ => false,
    };
    uuid_time_is_owning || timestamp_is_owning
}

fn uuid_v7_timestamp_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.get(8) == Some(&b'-')
        && bytes.get(13) == Some(&b'-')
        && bytes.get(14) == Some(&b'7')
        && bytes.get(18) == Some(&b'-')
        && bytes.get(23) == Some(&b'-');
    if !valid {
        return None;
    }

    let timestamp = format!("{}{}", &value[..8], &value[9..13]);
    u64::from_str_radix(&timestamp, 16).ok()
}

fn parse_rate_observation(
    value: &Map<String, Value>,
    timestamp: DateTime<Utc>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<RateObservation> {
    let primary = value.get("primary").and_then(parse_limit_window);
    let secondary = value.get("secondary").and_then(parse_limit_window);
    let limit_id = string_field_in(value, &["limit_id", "limitId"])
        .unwrap_or("codex")
        .to_owned();

    if primary.is_none()
        && secondary.is_none()
        && !value.contains_key("limit_id")
        && !value.contains_key("limitId")
    {
        return None;
    }

    Some(RateObservation {
        timestamp,
        thread_id: thread_id.to_owned(),
        turn_id: turn_id.map(str::to_owned),
        limit_id,
        primary,
        secondary,
        provenance: Provenance::LocalExact,
    })
}

fn parse_limit_window(value: &Value) -> Option<LimitWindow> {
    let value = value.as_object()?;
    let used_percent = f64_field_in(value, &["used_percent", "usedPercent"])?;
    let duration = i64_field_in(
        value,
        &[
            "window_minutes",
            "windowMinutes",
            "window_duration_mins",
            "windowDurationMins",
        ],
    );
    let resets_at = value
        .get("resets_at")
        .or_else(|| value.get("resetsAt"))
        .and_then(parse_timestamp);
    Some(LimitWindow::new(used_percent, duration, resets_at))
}

fn parse_token_usage(value: &Value) -> Option<TokenUsage> {
    let value = value.as_object()?;
    let known_fields = [
        "input_tokens",
        "inputTokens",
        "cached_input_tokens",
        "cachedInputTokens",
        "output_tokens",
        "outputTokens",
        "reasoning_output_tokens",
        "reasoningOutputTokens",
        "total_tokens",
        "totalTokens",
    ];
    if !known_fields.iter().any(|field| value.contains_key(*field)) {
        return None;
    }

    let input_tokens = u64_field_in(value, &["input_tokens", "inputTokens"]).unwrap_or(0);
    let cached_input_tokens =
        u64_field_in(value, &["cached_input_tokens", "cachedInputTokens"]).unwrap_or(0);
    let output_tokens = u64_field_in(value, &["output_tokens", "outputTokens"]).unwrap_or(0);
    let reasoning_output_tokens =
        u64_field_in(value, &["reasoning_output_tokens", "reasoningOutputTokens"]).unwrap_or(0);
    let total_tokens = u64_field_in(value, &["total_tokens", "totalTokens"])
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    })
}

fn finish_dataset(
    config: &CollectConfig,
    now: DateTime<Utc>,
    threads: HashMap<String, ThreadBuilder>,
    thread_titles: &HashMap<String, String>,
    dataset: &mut RolloutDataset,
) {
    for mut thread in threads.into_values() {
        let active_is_fresh = !thread.active_turn_ids.is_empty()
            && timestamp_is_fresh(thread.updated_at, now, config.active_grace);
        for active_turn_id in &thread.active_turn_ids {
            if let Some(turn) = thread.turns.get_mut(active_turn_id) {
                turn.status = if active_is_fresh {
                    TurnStatus::InProgress
                } else {
                    TurnStatus::Stale
                };
            }
        }

        let (status, status_provenance, status_confidence) = task_status(&thread, active_is_fresh);
        let turn_count = thread.turns.len();
        for turn in thread.turns.into_values() {
            let duration_ms = turn.duration_ms.or_else(|| {
                turn.started_at
                    .zip(turn.completed_at)
                    .and_then(|(start, end)| {
                        end.signed_duration_since(start)
                            .num_milliseconds()
                            .try_into()
                            .ok()
                    })
            });
            dataset.turns.push(TurnRecord {
                thread_id: thread.thread_id.clone(),
                turn_id: turn.turn_id,
                model: turn.model,
                reasoning_effort: turn.reasoning_effort,
                service_tier: turn.service_tier,
                message_preview: turn.message_preview,
                started_at: turn.started_at,
                completed_at: turn.completed_at,
                duration_ms,
                status: turn.status,
                token_usage: turn.token_usage,
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
            });
        }

        let title = if config.redact_content {
            "[redacted]".to_owned()
        } else {
            thread_titles
                .get(&thread.thread_id)
                .cloned()
                .or(thread.title)
                .unwrap_or_else(|| "Untitled task".to_owned())
        };
        dataset.tasks.push(TaskRecord {
            thread_id: thread.thread_id,
            title,
            cwd: thread.cwd,
            source: thread.source,
            created_at: thread.created_at,
            updated_at: thread.updated_at,
            status,
            status_provenance,
            status_confidence,
            token_usage: thread.token_usage,
            turn_count,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
        });
    }

    dataset.tasks.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    dataset.turns.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
    dataset.calls.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
    dataset.rate_observations.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
}

fn task_status(
    thread: &ThreadBuilder,
    active_is_fresh: bool,
) -> (TaskStatus, Provenance, Confidence) {
    if !thread.active_turn_ids.is_empty() {
        return if active_is_fresh {
            (
                TaskStatus::Running,
                Provenance::Inferred,
                Confidence::Medium,
            )
        } else {
            (TaskStatus::Stale, Provenance::Stale, Confidence::Low)
        };
    }

    let latest = thread.turns.values().max_by_key(|turn| {
        turn.completed_at
            .or(turn.started_at)
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    });
    match latest.map(|turn| turn.status) {
        Some(TurnStatus::Completed) => (
            TaskStatus::Completed,
            Provenance::LocalExact,
            Confidence::High,
        ),
        Some(TurnStatus::Interrupted) => (
            TaskStatus::Interrupted,
            Provenance::LocalExact,
            Confidence::High,
        ),
        Some(TurnStatus::Failed) => (TaskStatus::Failed, Provenance::LocalExact, Confidence::High),
        Some(TurnStatus::Stale | TurnStatus::InProgress) => {
            (TaskStatus::Stale, Provenance::Stale, Confidence::Low)
        }
        Some(TurnStatus::Unknown) | None => {
            (TaskStatus::Idle, Provenance::Inferred, Confidence::Low)
        }
    }
}

fn ensure_turn<'a>(thread: &'a mut ThreadBuilder, turn_id: &str) -> &'a mut TurnBuilder {
    thread
        .turns
        .entry(turn_id.to_owned())
        .or_insert_with(|| TurnBuilder {
            turn_id: turn_id.to_owned(),
            ..TurnBuilder::default()
        })
}

fn timestamp_is_fresh(
    timestamp: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    grace: std::time::Duration,
) -> bool {
    let Some(timestamp) = timestamp else {
        return false;
    };
    if timestamp >= now {
        return true;
    }
    now.signed_duration_since(timestamp)
        .to_std()
        .map(|age| age <= grace)
        .unwrap_or(false)
}

fn title_preview(message: &str) -> Option<String> {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(shorten_preview(&normalized, 96))
}

fn shorten_preview(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_owned();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut preview = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    preview.push_str("...");
    preview
}

fn source_label(value: &Value) -> Option<String> {
    match value {
        Value::String(source) => Some(source.clone()),
        Value::Object(source) => source.keys().next().cloned(),
        _ => None,
    }
}

fn session_source_label(payload: &Map<String, Value>) -> Option<String> {
    let thread_source = string_field_in(payload, &["thread_source", "threadSource"]);
    let source = payload.get("source");
    let source_is_subagent = source
        .and_then(Value::as_object)
        .is_some_and(|source| source.contains_key("subagent") || source.contains_key("subAgent"));
    if thread_source.is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
        || source_is_subagent
    {
        return Some("subagent".to_string());
    }

    if let Some(originator) = string_field_in(payload, &["originator"])
        .map(str::trim)
        .filter(|originator| !originator.is_empty())
    {
        return Some(if originator.eq_ignore_ascii_case("Codex Desktop") {
            "desktop".to_string()
        } else if originator.eq_ignore_ascii_case("codex-tui") {
            "cli".to_string()
        } else {
            originator.to_string()
        });
    }

    source.and_then(source_label).or_else(|| {
        thread_source
            .filter(|source| !source.eq_ignore_ascii_case("user"))
            .map(str::to_owned)
    })
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(timestamp) = value.as_str() {
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(timestamp) {
            return Some(timestamp.with_timezone(&Utc));
        }
        if let Ok(timestamp) = timestamp.parse::<i64>() {
            return timestamp_from_integer(timestamp);
        }
        return None;
    }
    if let Some(timestamp) = value.as_i64() {
        return timestamp_from_integer(timestamp);
    }
    value.as_f64().and_then(|timestamp| {
        let seconds = timestamp.trunc() as i64;
        let nanos = (timestamp.fract().abs() * 1_000_000_000.0) as u32;
        DateTime::from_timestamp(seconds, nanos)
    })
}

fn timestamp_from_integer(timestamp: i64) -> Option<DateTime<Utc>> {
    if timestamp.unsigned_abs() >= 100_000_000_000 {
        DateTime::from_timestamp_millis(timestamp)
    } else {
        DateTime::from_timestamp(timestamp, 0)
    }
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut value = value;
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn object_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Map<String, Value>> {
    value_at(value, path)?.as_object()
}

fn string_field<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    value
        .as_object()
        .and_then(|value| string_field_in(value, fields))
}

fn string_field_in<'a>(value: &'a Map<String, Value>, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(Value::as_str))
}

fn u64_field_in(value: &Map<String, Value>, fields: &[&str]) -> Option<u64> {
    fields.iter().find_map(|field| {
        value.get(*field).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn i64_field_in(value: &Map<String, Value>, fields: &[&str]) -> Option<i64> {
    fields.iter().find_map(|field| {
        value.get(*field).and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn f64_field_in(value: &Map<String, Value>, fields: &[&str]) -> Option<f64> {
    fields.iter().find_map(|field| {
        value.get(*field).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn set_min_timestamp(slot: &mut Option<DateTime<Utc>>, value: DateTime<Utc>) {
    if slot.is_none_or(|current| value < current) {
        *slot = Some(value);
    }
}

fn set_max_timestamp(slot: &mut Option<DateTime<Utc>>, value: DateTime<Utc>) {
    if slot.is_none_or(|current| value > current) {
        *slot = Some(value);
    }
}
