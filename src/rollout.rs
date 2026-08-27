use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::cache::write_private_atomically;
use crate::config::CollectConfig;
use crate::domain::{
    CollectionStats, Confidence, LimitWindow, Provenance, RateObservation, RolloutDataset,
    TaskRecord, TaskStatus, TokenUsage, TurnRecord, TurnStatus, UsageCall,
};
use crate::session_index::load_thread_titles;

const TURN_MESSAGE_PREVIEW_CHARS: usize = 72;
const ROLLOUT_CACHE_FORMAT_VERSION: u32 = 1;
// Bump when the projected event schema or replay semantics change.
const ROLLOUT_PARSER_REVISION: u32 = 4;
const ROLLOUT_CACHE_DIRECTORY: &str = "rollouts-v1";
const MAX_PERSISTENT_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PERSISTENT_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PERSISTENT_CACHE_ENTRIES: usize = 2_000;
const PERSISTENT_WRITE_DEBOUNCE: Duration = Duration::from_secs(30);
const PERSISTENT_WRITE_RETRY_INITIAL: Duration = Duration::from_secs(30);
const PERSISTENT_WRITE_RETRY_MAX: Duration = Duration::from_secs(15 * 60);
const PERSISTENT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const STALE_CACHE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const DISCOVERY_FULL_RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_INCOMPLETE_RESCAN_INTERVAL: Duration = Duration::from_secs(10);
const TAIL_GUARD_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RolloutFile {
    path: PathBuf,
    modified_at: DateTime<Utc>,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
}

impl FileFingerprint {
    fn from_metadata(metadata: &Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt;

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
            #[cfg(windows)]
            creation_time: metadata.creation_time(),
            #[cfg(windows)]
            last_write_time: metadata.last_write_time(),
        })
    }

    fn same_source_identity(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            return self.dev == other.dev && self.ino == other.ino;
        }
        #[cfg(windows)]
        {
            return self.creation_time == other.creation_time;
        }
        #[allow(unreachable_code)]
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedFile {
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum ParsedEvent {
    SessionMeta {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    ForeignCounterBaseline(TokenUsage),
    UserMessage {
        preview: String,
        turn_id: Option<String>,
        source: UserMessageSource,
    },
    ThreadSettingsApplied {
        service_tier: String,
    },
    TurnContext {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    TaskStarted {
        timestamp: DateTime<Utc>,
        turn_id: String,
        started_at: Option<DateTime<Utc>>,
    },
    TaskComplete {
        timestamp: DateTime<Utc>,
        turn_id: String,
        completed_at: Option<DateTime<Utc>>,
        duration_ms: Option<u64>,
    },
    TurnAborted {
        timestamp: DateTime<Utc>,
        turn_id: String,
        completed_at: Option<DateTime<Utc>>,
        duration_ms: Option<u64>,
        failed: bool,
    },
    TokenCount {
        timestamp: DateTime<Utc>,
        line_number: usize,
        total_usage: Option<TokenUsage>,
        #[serde(default)]
        last_usage: Option<TokenUsage>,
        rate_limits: Option<CachedRateLimits>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum UserMessageSource {
    ResponseItem,
    EventMessage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CachedRateLimits {
    limit_id: String,
    primary: Option<LimitWindow>,
    secondary: Option<LimitWindow>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TokenCounterSample {
    total: Option<TokenUsage>,
    last: Option<TokenUsage>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TailGuard {
    len: u16,
    hash: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ParsedFile {
    owner_thread_id: Option<String>,
    activity_updated_at: Option<DateTime<Utc>>,
    events: Vec<ParsedEvent>,
    parsed_lines: usize,
    skipped_lines: usize,
    unreadable_files: usize,
    warnings: Vec<String>,
    complete: bool,
    #[serde(default)]
    source_lines: usize,
    #[serde(default)]
    owning_created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    replaying_foreign_history: bool,
    #[serde(default)]
    ends_with_newline: bool,
    #[serde(default)]
    tail_guard: TailGuard,
    #[serde(default)]
    foreign_baseline_events: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedFile {
    fingerprint: FileFingerprint,
    parsed: ParsedFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CacheKey {
    codex_home: PathBuf,
    redact_content: bool,
}

#[derive(Clone, Debug, Default)]
struct ReducedRollouts {
    threads: HashMap<String, ThreadBuilder>,
    dataset: RolloutDataset,
    replay_warnings: HashMap<PathBuf, Vec<String>>,
    ambiguous_token_resets: HashMap<PathBuf, usize>,
}

/// Diagnostics for the most recent cached scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RolloutCacheRefresh {
    /// Whether this refresh walked the complete rollout directory tree.
    pub discovery_full_scan: bool,
    /// Whether the in-memory discovery inventory was reused.
    pub discovery_cache_hit: bool,
    /// Whether a cached inventory was discarded after a metadata/config check.
    pub discovery_invalidated: bool,
    /// Rollout files probed directly during discovery.
    pub discovery_probed_files: usize,
    /// Rollout directories probed directly during discovery.
    pub discovery_probed_dirs: usize,
    pub reused_files: usize,
    pub reparsed_files: usize,
    /// Changed append-only files parsed from their previous byte boundary.
    pub tail_parsed_files: usize,
    /// New bytes consumed by successful append-only tail parsing.
    pub tail_parsed_bytes: u64,
    /// Files parsed from byte zero, including new/replaced/truncated files.
    pub full_parsed_files: usize,
    /// Files hydrated from the user-level parsed rollout cache.
    pub disk_reused_files: usize,
    /// Persistent entries that were absent or stale for the current fingerprint.
    pub disk_misses: usize,
    /// Persistent entries that could not be decoded or validated.
    pub disk_corrupt_files: usize,
    /// Parsed entries successfully persisted during this refresh.
    pub disk_written_files: usize,
    /// Best-effort persistent writes that failed without affecting the snapshot.
    pub disk_write_failures: usize,
    /// Dirty entries deferred while a previous write failure is backing off.
    pub disk_deferred_files: usize,
    /// Entries that exceeded the per-file cache safety limit and were not written.
    pub disk_oversized_files: usize,
    /// Milliseconds until deferred persistent writes are eligible to retry.
    pub disk_write_retry_ms: u64,
    /// Old persistent entries removed by best-effort size pruning.
    pub disk_pruned_files: usize,
    /// Stale atomic-write temporary files removed during maintenance.
    pub disk_pruned_temp_files: usize,
    /// Number of files parsed a second time after changing during the first read.
    pub stability_retries: usize,
    pub rebuilt: bool,
    /// Threads replayed without rebuilding unrelated thread state.
    pub incrementally_reduced_threads: usize,
    /// Whether this refresh rebuilt all selected rollout state.
    pub full_rebuild: bool,
    pub discover_us: u64,
    pub cache_load_us: u64,
    pub parse_us: u64,
    pub cache_save_us: u64,
    pub reduce_us: u64,
    pub materialize_us: u64,
    /// Number of `session_index.jsonl` content-read attempts.
    pub session_index_reads: usize,
    /// Whether the cached session-title snapshot was reused unchanged.
    pub session_index_reused: bool,
}

/// Cheap, content-free counters for runtime performance diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RolloutCacheMetrics {
    pub selected_files: usize,
    pub selected_bytes: u64,
    pub parsed_lines: usize,
    pub cached_events: usize,
    pub foreign_baseline_events: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryState {
    stats: CollectionStats,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryKey {
    codex_home: PathBuf,
    lookback_days: i64,
    max_files: usize,
}

#[derive(Debug)]
struct DiscoveryCache {
    key: DiscoveryKey,
    inventory: HashMap<PathBuf, RolloutFile>,
    directories: HashMap<PathBuf, FileFingerprint>,
    roots: Vec<(PathBuf, Option<FileFingerprint>)>,
    warnings: Vec<String>,
    unreadable_files: usize,
    complete: bool,
    full_scan_at: Instant,
    logical_now: DateTime<Utc>,
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
    dirty_files: HashSet<PathBuf>,
    unpersistable_files: HashMap<PathBuf, u64>,
    disk_last_write: HashMap<PathBuf, Instant>,
    disk_write_retry_after: Option<Instant>,
    disk_write_backoff: Duration,
    last_disk_prune: Option<Instant>,
    last_refresh: RolloutCacheRefresh,
    metrics: RolloutCacheMetrics,
    last_materialized_at: Option<DateTime<Utc>>,
    last_materialized_active_grace: Option<Duration>,
    last_discovery: Option<DiscoveryState>,
    discovery_cache: Option<DiscoveryCache>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistentFileEntry {
    format_version: u32,
    parser_revision: u32,
    key: CacheKey,
    source_path: PathBuf,
    cached: CachedFile,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistentFileEntryRef<'a> {
    format_version: u32,
    parser_revision: u32,
    key: &'a CacheKey,
    source_path: &'a Path,
    cached: &'a CachedFile,
}

enum PersistentLoad {
    Hit(Box<CachedFile>),
    Miss,
    Corrupt,
}

enum PersistentWriteError {
    Oversized,
    Other,
}

#[derive(Default)]
struct PersistentWriteSummary {
    written: usize,
    failures: usize,
    deferred: usize,
    oversized: usize,
    retry_ms: u64,
    pruned_entries: usize,
    pruned_temps: usize,
    maintenance_ran: bool,
}

#[derive(Clone, Copy, Default)]
struct PersistentCacheUsage {
    entries: usize,
    bytes: u64,
}

#[derive(Default)]
struct PersistentPruneSummary {
    entries: usize,
    stale_temps: usize,
    usage: PersistentCacheUsage,
}

struct PersistentCacheBudget {
    directory: PathBuf,
    usage: PersistentCacheUsage,
    max_entries: usize,
    max_bytes: u64,
    pruned_entries: usize,
    pruned_temps: usize,
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedBuffer {
    fn new(limit: u64) -> Self {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        Self {
            bytes: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "parsed rollout cache entry exceeds the safety limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PersistentCacheBudget {
    fn new(directory: PathBuf, usage: PersistentCacheUsage) -> Self {
        Self {
            directory,
            usage,
            max_entries: MAX_PERSISTENT_CACHE_ENTRIES,
            max_bytes: MAX_PERSISTENT_CACHE_BYTES,
            pruned_entries: 0,
            pruned_temps: 0,
        }
    }

    fn reserve(&mut self, target: &Path, new_bytes: u64) -> std::io::Result<Option<u64>> {
        let old_bytes = existing_cache_file_len(target);
        if self.can_write_atomically(old_bytes, new_bytes) {
            return Ok(old_bytes);
        }

        let added_entries = usize::from(old_bytes.is_none());
        let pruned = prune_cache_directory(
            &self.directory,
            Some(target),
            self.max_entries.saturating_sub(added_entries),
            // Atomic replacement temporarily keeps both the old target and
            // the complete new temporary file on disk.
            self.max_bytes.saturating_sub(new_bytes),
            stale_cache_temp_cutoff(),
        );
        self.usage = pruned.usage;
        self.pruned_entries += pruned.entries;
        self.pruned_temps += pruned.stale_temps;

        let old_bytes = existing_cache_file_len(target);
        if self.can_write_atomically(old_bytes, new_bytes) {
            Ok(old_bytes)
        } else {
            Err(std::io::Error::other(
                "could not reserve space within the persistent cache limit",
            ))
        }
    }

    fn commit(&mut self, old_bytes: Option<u64>, new_bytes: u64) {
        self.usage.entries = self
            .usage
            .entries
            .saturating_sub(usize::from(old_bytes.is_some()))
            .saturating_add(1);
        self.usage.bytes = self
            .usage
            .bytes
            .saturating_sub(old_bytes.unwrap_or(0))
            .saturating_add(new_bytes);
    }

    fn projected_usage(&self, old_bytes: Option<u64>, new_bytes: u64) -> PersistentCacheUsage {
        PersistentCacheUsage {
            entries: self
                .usage
                .entries
                .saturating_sub(usize::from(old_bytes.is_some()))
                .saturating_add(1),
            bytes: self
                .usage
                .bytes
                .saturating_sub(old_bytes.unwrap_or(0))
                .saturating_add(new_bytes),
        }
    }

    fn can_write_atomically(&self, old_bytes: Option<u64>, new_bytes: u64) -> bool {
        self.projected_usage(old_bytes, new_bytes)
            .is_within(self.max_entries, self.max_bytes)
            && self.usage.bytes.saturating_add(new_bytes) <= self.max_bytes
    }
}

impl PersistentCacheUsage {
    fn is_within(self, max_entries: usize, max_bytes: u64) -> bool {
        self.entries <= max_entries && self.bytes <= max_bytes
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ParentThreadRank {
    Fork,
    Direct,
    Nested,
}

#[derive(Clone, Debug, Default)]
struct ThreadBuilder {
    thread_id: String,
    parent_thread_id: Option<String>,
    parent_thread_rank: Option<ParentThreadRank>,
    seen_active_file: bool,
    seen_archived_file: bool,
    title: Option<String>,
    title_source: Option<UserMessageSource>,
    title_turn_id: Option<String>,
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
    message_source: Option<UserMessageSource>,
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

    pub fn metrics(&self) -> RolloutCacheMetrics {
        self.metrics
    }

    fn discover_rollout_files(
        &mut self,
        config: &CollectConfig,
        now: DateTime<Utc>,
        dataset: &mut RolloutDataset,
        refresh: &mut RolloutCacheRefresh,
    ) -> Vec<RolloutFile> {
        let key = DiscoveryKey {
            codex_home: config.codex_home.clone(),
            lookback_days: config.lookback_days,
            max_files: config.max_files,
        };
        let selected = self.selected.clone();
        let had_cache = self.discovery_cache.is_some();
        let mut cache_hit = false;
        let mut cache_invalidated = false;

        if let Some(cache) = self.discovery_cache.as_mut() {
            let rescan_interval = if cache.complete {
                DISCOVERY_FULL_RESCAN_INTERVAL
            } else {
                DISCOVERY_INCOMPLETE_RESCAN_INTERVAL
            };
            let due = cache.full_scan_at.elapsed() >= rescan_interval;
            let key_changed = cache.key != key;
            let clock_rolled_back = now < cache.logical_now;
            if !due && !key_changed && !clock_rolled_back {
                if cache.complete {
                    match refresh_cached_discovery(cache, &selected, now, refresh) {
                        Ok(()) => cache_hit = true,
                        Err(()) => cache_invalidated = true,
                    }
                } else {
                    // Preserve the warning-bearing partial inventory without
                    // repeating an expensive failing walk every two seconds,
                    // while still following active selected-file appends.
                    match refresh_selected_rollout_files(cache, &selected, now, refresh) {
                        Ok(()) => cache_hit = true,
                        Err(()) => cache_invalidated = true,
                    }
                }
            } else if key_changed || clock_rolled_back {
                cache_invalidated = true;
            }
        }

        if !cache_hit {
            self.discovery_cache = Some(discover_rollout_inventory(config, now, &key, refresh));
            refresh.discovery_full_scan = true;
            refresh.discovery_invalidated = had_cache && cache_invalidated;
        } else {
            refresh.discovery_cache_hit = true;
        }

        let cache = self
            .discovery_cache
            .as_ref()
            .expect("discovery always initializes its inventory");
        select_rollout_files(cache, config, now, dataset)
    }

    /// Scans recent rollouts, reparsing rollout files and reloading the session
    /// title index only when their metadata fingerprints change.
    pub fn scan(&mut self, config: &CollectConfig, now: DateTime<Utc>) -> Result<RolloutDataset> {
        Ok(self
            .scan_inner(config, now, true)?
            .expect("a forced rollout scan always materializes a dataset"))
    }

    /// Returns a new dataset only when rollout/title/discovery state changed or
    /// an active task crossed its freshness deadline since the prior result.
    pub fn scan_if_changed(
        &mut self,
        config: &CollectConfig,
        now: DateTime<Utc>,
    ) -> Result<Option<RolloutDataset>> {
        self.scan_inner(config, now, false)
    }

    fn scan_inner(
        &mut self,
        config: &CollectConfig,
        now: DateTime<Utc>,
        force_materialize: bool,
    ) -> Result<Option<RolloutDataset>> {
        let trace_active = config.startup_trace.is_active();
        let perf_active = config.perf_log.is_enabled();
        let scan_started = trace_active.then(Instant::now);
        let mut refresh = RolloutCacheRefresh::default();
        let key = CacheKey {
            codex_home: config.codex_home.clone(),
            redact_content: config.redact_content,
        };
        if self.key.as_ref() != Some(&key) {
            self.files.clear();
            self.selected.clear();
            self.reduced = None;
            self.session_titles = SessionTitleCache::default();
            self.dirty_files.clear();
            self.unpersistable_files.clear();
            self.disk_last_write.clear();
            self.disk_write_retry_after = None;
            self.disk_write_backoff = Duration::ZERO;
            self.last_disk_prune = None;
            self.metrics = RolloutCacheMetrics::default();
            self.last_materialized_at = None;
            self.last_materialized_active_grace = None;
            self.last_discovery = None;
            self.discovery_cache = None;
            self.key = Some(key.clone());
        }

        let mut discovery = RolloutDataset::default();
        let discovery_started = perf_active.then(Instant::now);
        let discovery_span = config.startup_trace.span("rollout.discover");
        let mut files = self.discover_rollout_files(config, now, &mut discovery, &mut refresh);
        let (selected_bytes, largest_file_bytes) = files
            .iter()
            .map(|file| file.fingerprint.len)
            .fold((0_u64, 0_u64), |(total, largest), bytes| {
                (total.saturating_add(bytes), largest.max(bytes))
            });
        refresh.discover_us = elapsed_micros(discovery_started);
        discovery_span.finish_with(|| format!(
            "full={} cache_hit={} invalidated={} probed_files={} probed_dirs={} discovered={} selected={} truncated={} bytes={selected_bytes} largest_bytes={largest_file_bytes}",
            refresh.discovery_full_scan,
            refresh.discovery_cache_hit,
            refresh.discovery_invalidated,
            refresh.discovery_probed_files,
            refresh.discovery_probed_dirs,
            discovery.stats.discovered_files,
            files.len(),
            discovery.stats.truncated_files
        ));

        // Parsing older files first preserves cumulative counter order when a
        // thread happens to span more than one rollout file.
        let sort_span = config.startup_trace.span("rollout.sort_selected");
        files.sort_by(|left, right| {
            left.modified_at
                .cmp(&right.modified_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        sort_span.finish_with(|| format!("files={}", files.len()));

        let title_span = config.startup_trace.span("rollout.session_titles");
        self.refresh_session_titles(config, &mut discovery, &mut refresh);
        title_span.finish_with(|| {
            format!(
                "reads={} reused={} titles={} redacted={}",
                refresh.session_index_reads,
                refresh.session_index_reused,
                self.session_titles.titles.len(),
                config.redact_content
            )
        });

        let maintenance_due = self
            .last_disk_prune
            .is_none_or(|last| last.elapsed() >= PERSISTENT_MAINTENANCE_INTERVAL);
        let mut cache_usage = None;
        let maintenance_span = config.startup_trace.span("rollout.cache_maintenance");
        if config.rollout_cache_dir.is_some() && maintenance_due {
            let pruned = prune_persistent_files(config, &key, None);
            refresh.disk_pruned_files = pruned.entries;
            refresh.disk_pruned_temp_files = pruned.stale_temps;
            cache_usage = Some(pruned.usage);
            self.last_disk_prune = Some(Instant::now());
        }
        maintenance_span.finish_with(|| {
            format!(
                "enabled={} due={} pruned={} temp_pruned={}",
                config.rollout_cache_dir.is_some(),
                maintenance_due,
                refresh.disk_pruned_files,
                refresh.disk_pruned_temp_files
            )
        });

        let cache_load_started = perf_active.then(Instant::now);
        let cache_load_span = config.startup_trace.span("rollout.cache_load");
        if let Some(cache_root) = config.rollout_cache_dir.as_deref() {
            for file in &files {
                if self.files.contains_key(&file.path) {
                    continue;
                }
                match load_persistent_file(cache_root, &key, file) {
                    PersistentLoad::Hit(cached) => {
                        self.files.insert(file.path.clone(), *cached);
                        refresh.disk_reused_files += 1;
                    }
                    PersistentLoad::Miss => refresh.disk_misses += 1,
                    PersistentLoad::Corrupt => refresh.disk_corrupt_files += 1,
                }
            }
        }
        refresh.cache_load_us = elapsed_micros(cache_load_started);
        cache_load_span.finish_with(|| {
            format!(
                "enabled={} loaded={} misses={} corrupt={}",
                config.rollout_cache_dir.is_some(),
                refresh.disk_reused_files,
                refresh.disk_misses,
                refresh.disk_corrupt_files
            )
        });

        let parse_started = perf_active.then(Instant::now);
        let parse_span = config.startup_trace.span("rollout.parse_files");
        let mut slowest_parse_us = 0_u128;
        let mut slowest_file_bytes = 0_u64;
        let mut changed_thread_ids = HashSet::new();
        for file in &files {
            let reusable = self.files.get(&file.path).is_some_and(|cached| {
                cached.fingerprint == file.fingerprint && cached.parsed.complete
            });
            if reusable {
                refresh.reused_files += 1;
                continue;
            }

            let previous = self.files.remove(&file.path);
            if let Some(thread_id) = previous
                .as_ref()
                .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
            {
                changed_thread_ids.insert(thread_id.clone());
            }
            let file_started = trace_active.then(Instant::now);
            let parsed = refresh_stable_rollout_file(file, config, previous);
            let cached = parsed.cached;
            if let Some(file_started) = file_started {
                let parse_us = file_started.elapsed().as_micros();
                if parse_us > slowest_parse_us {
                    slowest_parse_us = parse_us;
                    slowest_file_bytes = file.fingerprint.len;
                }
            }
            if let Some(thread_id) = cached.parsed.owner_thread_id.as_ref() {
                changed_thread_ids.insert(thread_id.clone());
            }
            if parsed.tail_parsed {
                refresh.tail_parsed_files += 1;
                refresh.tail_parsed_bytes = refresh
                    .tail_parsed_bytes
                    .saturating_add(parsed.parsed_bytes);
            } else {
                refresh.full_parsed_files += 1;
            }
            let cacheable = cached.parsed.complete;
            let fingerprint_len = cached.fingerprint.len;
            self.files.insert(file.path.clone(), cached);
            if cacheable && config.rollout_cache_dir.is_some() {
                let still_too_large = self
                    .unpersistable_files
                    .get(&file.path)
                    .is_some_and(|previous_len| fingerprint_len >= *previous_len);
                if still_too_large {
                    refresh.disk_oversized_files += 1;
                } else {
                    self.unpersistable_files.remove(&file.path);
                    self.dirty_files.insert(file.path.clone());
                }
            }
            refresh.reparsed_files += 1;
            refresh.stability_retries += parsed.stability_retries;
        }
        refresh.parse_us = elapsed_micros(parse_started);
        let (parsed_lines, cached_events, foreign_baseline_events) = files
            .iter()
            .filter_map(|file| self.files.get(&file.path))
            .fold((0_usize, 0_usize, 0_usize), |totals, cached| {
                (
                    totals.0.saturating_add(cached.parsed.parsed_lines),
                    totals.1.saturating_add(cached.parsed.events.len()),
                    totals
                        .2
                        .saturating_add(cached.parsed.foreign_baseline_events),
                )
            });
        self.metrics = RolloutCacheMetrics {
            selected_files: files.len(),
            selected_bytes,
            parsed_lines,
            cached_events,
            foreign_baseline_events,
        };
        parse_span.finish_with(|| format!(
            "files={} reparsed={} tail_parsed={} tail_bytes={} full_parsed={} reused={} disk_reused={} retries={} bytes={selected_bytes} lines={parsed_lines} slowest_us={slowest_parse_us} slowest_bytes={slowest_file_bytes}",
            files.len(),
            refresh.reparsed_files,
            refresh.tail_parsed_files,
            refresh.tail_parsed_bytes,
            refresh.full_parsed_files,
            refresh.reused_files,
            refresh.disk_reused_files,
            refresh.stability_retries
        ));

        let cache_save_started = perf_active.then(Instant::now);
        let cache_save_span = config.startup_trace.span("rollout.cache_save");
        let write = self.persist_dirty_files(config, &key, cache_usage);
        refresh.disk_written_files = write.written;
        refresh.disk_write_failures = write.failures;
        refresh.disk_deferred_files = write.deferred;
        refresh.disk_oversized_files += write.oversized;
        refresh.disk_write_retry_ms = write.retry_ms;
        refresh.disk_pruned_files += write.pruned_entries;
        refresh.disk_pruned_temp_files += write.pruned_temps;
        if write.maintenance_ran {
            self.last_disk_prune = Some(Instant::now());
        }
        refresh.cache_save_us = elapsed_micros(cache_save_started);
        cache_save_span.finish_with(|| {
            format!(
                "enabled={} written={} failures={} deferred={} oversized={} retry_ms={} pruned={} temp_pruned={}",
                config.rollout_cache_dir.is_some(),
                refresh.disk_written_files,
                refresh.disk_write_failures,
                refresh.disk_deferred_files,
                refresh.disk_oversized_files,
                refresh.disk_write_retry_ms,
                refresh.disk_pruned_files,
                refresh.disk_pruned_temp_files
            )
        });

        let selected = files
            .iter()
            .filter_map(|file| {
                self.files.get(&file.path).map(|cached| SelectedFile {
                    path: file.path.clone(),
                    fingerprint: cached.fingerprint.clone(),
                })
            })
            .collect::<Vec<_>>();
        {
            let previous_by_path = self
                .selected
                .iter()
                .map(|file| (file.path.as_path(), &file.fingerprint))
                .collect::<HashMap<_, _>>();
            let current_by_path = selected
                .iter()
                .map(|file| (file.path.as_path(), &file.fingerprint))
                .collect::<HashMap<_, _>>();
            for previous in &self.selected {
                if current_by_path
                    .get(previous.path.as_path())
                    .is_none_or(|fingerprint| **fingerprint != previous.fingerprint)
                    && let Some(thread_id) = self
                        .files
                        .get(&previous.path)
                        .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
                {
                    changed_thread_ids.insert(thread_id.clone());
                }
            }
            for current in &selected {
                if previous_by_path
                    .get(current.path.as_path())
                    .is_none_or(|fingerprint| **fingerprint != current.fingerprint)
                    && let Some(thread_id) = self
                        .files
                        .get(&current.path)
                        .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
                {
                    changed_thread_ids.insert(thread_id.clone());
                }
            }
        }
        mark_threads_with_changed_file_order(
            &self.selected,
            &selected,
            &self.files,
            &mut changed_thread_ids,
        );
        let selected_changed = selected != self.selected;
        let must_rebuild = self.reduced.is_none() || selected_changed || refresh.reparsed_files > 0;

        let reduce_started = perf_active.then(Instant::now);
        let reduce_span = config.startup_trace.span("rollout.reduce");
        if must_rebuild {
            if let Some(reduced) = self.reduced.as_mut() {
                incrementally_reduce_cached_files(
                    reduced,
                    &changed_thread_ids,
                    &files,
                    &self.files,
                    config,
                );
                refresh.incrementally_reduced_threads = changed_thread_ids.len();
            } else {
                self.reduced = Some(reduce_cached_files(&files, &self.files, config));
                refresh.full_rebuild = true;
            }
            refresh.rebuilt = true;
        }
        refresh.reduce_us = elapsed_micros(reduce_started);
        let (reduced_threads, reduced_calls) = if trace_active {
            self.reduced
                .as_ref()
                .map(|reduced| (reduced.threads.len(), reduced.dataset.calls.len()))
                .unwrap_or_default()
        } else {
            (0, 0)
        };
        reduce_span.finish_with(|| {
            format!(
                "rebuilt={} full_rebuild={} incremental_threads={} threads={reduced_threads} calls={reduced_calls}",
                refresh.rebuilt,
                refresh.full_rebuild,
                refresh.incrementally_reduced_threads
            )
        });
        self.selected = selected;

        let selected_paths = files
            .iter()
            .map(|file| file.path.as_path())
            .collect::<std::collections::HashSet<_>>();
        self.files
            .retain(|path, _| selected_paths.contains(path.as_path()));
        self.disk_last_write
            .retain(|path, _| selected_paths.contains(path.as_path()));
        self.unpersistable_files
            .retain(|path, _| selected_paths.contains(path.as_path()));
        self.dirty_files
            .retain(|path| selected_paths.contains(path.as_path()));
        let discovery_state = DiscoveryState {
            stats: discovery.stats.clone(),
            warnings: discovery.warnings.clone(),
        };
        let discovery_changed = self.last_discovery.as_ref() != Some(&discovery_state);
        let title_changed = !config.redact_content && !refresh.session_index_reused;
        let freshness_changed = self.last_materialized_at.is_some_and(|last| last > now)
            || self.last_materialized_active_grace != Some(config.active_grace)
            || self.last_materialized_at.is_some_and(|last| {
                crossed_freshness_deadline(
                    self.reduced
                        .as_ref()
                        .expect("a scan always initializes reduced rollout state"),
                    last,
                    now,
                    config.active_grace,
                )
            });
        let should_materialize = force_materialize
            || self.last_materialized_at.is_none()
            || refresh.rebuilt
            || discovery_changed
            || title_changed
            || freshness_changed;
        self.last_discovery = Some(discovery_state);
        if !should_materialize {
            self.last_refresh = refresh;
            return Ok(None);
        }

        let materialize_started = perf_active.then(Instant::now);
        let materialize_span = config.startup_trace.span("rollout.materialize");
        let dataset = materialize_dataset(
            self.reduced
                .as_ref()
                .expect("a scan always initializes reduced rollout state"),
            discovery,
            config,
            now,
            &self.session_titles.titles,
        );
        refresh.materialize_us = elapsed_micros(materialize_started);
        self.last_refresh = refresh;
        self.last_materialized_at = Some(now);
        self.last_materialized_active_grace = Some(config.active_grace);
        materialize_span.finish_with(|| {
            format!(
                "tasks={} turns={} calls={} observations={}",
                dataset.tasks.len(),
                dataset.turns.len(),
                dataset.calls.len(),
                dataset.rate_observations.len()
            )
        });
        if let Some(scan_started) = scan_started {
            config
                .startup_trace
                .record_with("rollout.total", scan_started, || {
                format!(
                "files={} lines={} bytes={selected_bytes} reparsed={} reused={} disk_reused={} disk_written={} disk_failures={} disk_deferred={} disk_oversized={} disk_retry_ms={} disk_pruned={} disk_temp_pruned={} retries={} rebuilt={}",
                dataset.stats.scanned_files,
                dataset.stats.parsed_lines,
                refresh.reparsed_files,
                refresh.reused_files,
                refresh.disk_reused_files,
                refresh.disk_written_files,
                refresh.disk_write_failures,
                refresh.disk_deferred_files,
                refresh.disk_oversized_files,
                refresh.disk_write_retry_ms,
                refresh.disk_pruned_files,
                refresh.disk_pruned_temp_files,
                refresh.stability_retries,
                refresh.rebuilt
                )
            });
        }
        Ok(Some(dataset))
    }

    fn persist_dirty_files(
        &mut self,
        config: &CollectConfig,
        key: &CacheKey,
        cache_usage: Option<PersistentCacheUsage>,
    ) -> PersistentWriteSummary {
        self.persist_dirty_files_inner(config, key, MAX_PERSISTENT_ENTRY_BYTES, cache_usage)
    }

    #[cfg(test)]
    fn persist_dirty_files_with_limit(
        &mut self,
        config: &CollectConfig,
        key: &CacheKey,
        max_entry_bytes: u64,
    ) -> PersistentWriteSummary {
        self.persist_dirty_files_inner(config, key, max_entry_bytes, None)
    }

    fn persist_dirty_files_inner(
        &mut self,
        config: &CollectConfig,
        key: &CacheKey,
        max_entry_bytes: u64,
        cache_usage: Option<PersistentCacheUsage>,
    ) -> PersistentWriteSummary {
        let Some(cache_root) = config.rollout_cache_dir.as_deref() else {
            self.dirty_files.clear();
            self.unpersistable_files.clear();
            self.disk_last_write.clear();
            self.disk_write_retry_after = None;
            self.disk_write_backoff = Duration::ZERO;
            return PersistentWriteSummary::default();
        };

        let mut summary = PersistentWriteSummary::default();
        if self.dirty_files.is_empty() {
            return summary;
        }

        let now = Instant::now();
        if let Some(retry_after) = self.disk_write_retry_after
            && let Some(remaining) = retry_after.checked_duration_since(now)
        {
            summary.deferred = self.dirty_files.len();
            summary.retry_ms = duration_millis(remaining);
            return summary;
        }
        self.disk_write_retry_after = None;
        let namespace = persistent_namespace_path(cache_root, key);
        let mut budget =
            cache_usage.map(|usage| PersistentCacheBudget::new(namespace.clone(), usage));

        let mut paths = std::mem::take(&mut self.dirty_files)
            .into_iter()
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if let Some(last_write) = self.disk_last_write.get(&path)
                && let Some(remaining) = PERSISTENT_WRITE_DEBOUNCE
                    .checked_sub(now.saturating_duration_since(*last_write))
                && !remaining.is_zero()
            {
                summary.deferred += 1;
                record_earliest_retry(&mut summary.retry_ms, remaining);
                self.dirty_files.insert(path);
                continue;
            }
            let Some(cached) = self
                .files
                .get(&path)
                .filter(|cached| cached.parsed.complete)
            else {
                continue;
            };
            if self
                .unpersistable_files
                .get(&path)
                .is_some_and(|previous_len| cached.fingerprint.len >= *previous_len)
            {
                summary.oversized += 1;
                continue;
            }
            self.unpersistable_files.remove(&path);
            let contents = match serialize_persistent_entry(key, &path, cached, max_entry_bytes) {
                Ok(contents) => contents,
                Err(PersistentWriteError::Oversized) => {
                    summary.oversized += 1;
                    self.unpersistable_files
                        .insert(path, cached.fingerprint.len);
                    continue;
                }
                Err(PersistentWriteError::Other) => {
                    summary.failures += 1;
                    self.dirty_files.insert(path);
                    continue;
                }
            };
            if budget.is_none() {
                let pruned = prune_persistent_files(config, key, None);
                summary.pruned_entries += pruned.entries;
                summary.pruned_temps += pruned.stale_temps;
                summary.maintenance_ran = true;
                budget = Some(PersistentCacheBudget::new(namespace.clone(), pruned.usage));
            }
            let budget = budget
                .as_mut()
                .expect("a persistent write always initializes its cache budget");
            let target = persistent_entry_path(cache_root, key, &path);
            let old_bytes = match budget.reserve(&target, contents.len() as u64) {
                Ok(old_bytes) => old_bytes,
                Err(_) => {
                    summary.failures += 1;
                    self.dirty_files.insert(path);
                    continue;
                }
            };
            if write_private_atomically(&target, &contents).is_err() {
                summary.failures += 1;
                self.dirty_files.insert(path);
                continue;
            }
            budget.commit(old_bytes, contents.len() as u64);
            summary.written += 1;
            self.disk_last_write.insert(path, Instant::now());
        }

        if let Some(budget) = budget {
            summary.pruned_entries += budget.pruned_entries;
            summary.pruned_temps += budget.pruned_temps;
        }

        if summary.failures > 0 {
            self.disk_write_backoff = next_write_backoff(self.disk_write_backoff);
            self.disk_write_retry_after = Instant::now().checked_add(self.disk_write_backoff);
            summary.retry_ms = duration_millis(self.disk_write_backoff);
        } else {
            self.disk_write_backoff = Duration::ZERO;
            self.disk_write_retry_after = None;
        }
        summary
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

fn load_persistent_file(cache_root: &Path, key: &CacheKey, file: &RolloutFile) -> PersistentLoad {
    let entry_path = persistent_entry_path(cache_root, key, &file.path);
    let metadata = match entry_path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return PersistentLoad::Miss,
        Err(_) => return PersistentLoad::Corrupt,
    };
    if !metadata.is_file() || metadata.len() > MAX_PERSISTENT_ENTRY_BYTES {
        return PersistentLoad::Corrupt;
    }
    let contents = match fs::read(&entry_path) {
        Ok(contents) => contents,
        Err(_) => return PersistentLoad::Corrupt,
    };
    let entry: PersistentFileEntry = match serde_json::from_slice(&contents) {
        Ok(entry) => entry,
        Err(_) => return PersistentLoad::Corrupt,
    };
    if entry.format_version != ROLLOUT_CACHE_FORMAT_VERSION
        || entry.parser_revision != ROLLOUT_PARSER_REVISION
        || entry.key != *key
        || entry.source_path != file.path
        || entry.cached.fingerprint != file.fingerprint
        || !entry.cached.parsed.complete
    {
        return PersistentLoad::Miss;
    }
    if inspect_rollout_file(&file.path)
        .map(|current| current.fingerprint != file.fingerprint)
        .unwrap_or(true)
    {
        return PersistentLoad::Miss;
    }
    PersistentLoad::Hit(Box::new(entry.cached))
}

#[cfg(test)]
fn persist_file_entry(
    cache_root: &Path,
    key: &CacheKey,
    source_path: &Path,
    cached: &CachedFile,
    max_entry_bytes: u64,
) -> std::result::Result<(), PersistentWriteError> {
    let contents = serialize_persistent_entry(key, source_path, cached, max_entry_bytes)?;
    write_private_atomically(
        &persistent_entry_path(cache_root, key, source_path),
        &contents,
    )
    .map_err(|_| PersistentWriteError::Other)
}

fn serialize_persistent_entry(
    key: &CacheKey,
    source_path: &Path,
    cached: &CachedFile,
    max_entry_bytes: u64,
) -> std::result::Result<Vec<u8>, PersistentWriteError> {
    let entry = PersistentFileEntryRef {
        format_version: ROLLOUT_CACHE_FORMAT_VERSION,
        parser_revision: ROLLOUT_PARSER_REVISION,
        key,
        source_path,
        cached,
    };
    let mut contents = LimitedBuffer::new(max_entry_bytes);
    if serde_json::to_writer(&mut contents, &entry).is_err() {
        return Err(if contents.exceeded {
            PersistentWriteError::Oversized
        } else {
            PersistentWriteError::Other
        });
    }
    Ok(contents.bytes)
}

fn next_write_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        PERSISTENT_WRITE_RETRY_INITIAL
    } else {
        current.saturating_mul(2).min(PERSISTENT_WRITE_RETRY_MAX)
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn elapsed_micros(started: Option<Instant>) -> u64 {
    started
        .map(|started| started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn record_earliest_retry(slot: &mut u64, duration: Duration) {
    let candidate = duration_millis(duration);
    if *slot == 0 || candidate < *slot {
        *slot = candidate;
    }
}

fn prune_persistent_files(
    config: &CollectConfig,
    key: &CacheKey,
    protected: Option<&Path>,
) -> PersistentPruneSummary {
    let Some(cache_root) = config.rollout_cache_dir.as_deref() else {
        return PersistentPruneSummary::default();
    };
    let directory = persistent_namespace_path(cache_root, key);
    prune_cache_directory(
        &directory,
        protected,
        MAX_PERSISTENT_CACHE_ENTRIES,
        MAX_PERSISTENT_CACHE_BYTES,
        stale_cache_temp_cutoff(),
    )
}

fn prune_cache_directory(
    directory: &Path,
    protected: Option<&Path>,
    max_entries: usize,
    max_bytes: u64,
    stale_temp_before: SystemTime,
) -> PersistentPruneSummary {
    let Ok(entries) = fs::read_dir(directory) else {
        return PersistentPruneSummary::default();
    };
    let mut cache_entries = Vec::new();
    let mut summary = PersistentPruneSummary::default();
    for entry in entries.filter_map(|entry| entry.ok()) {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            cache_entries.push((path, metadata.len(), modified));
        } else if is_cache_temporary_file(path.file_name())
            && modified <= stale_temp_before
            && fs::remove_file(&path).is_ok()
        {
            summary.stale_temps += 1;
        }
    }
    let mut entries = cache_entries;
    entries.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
    let mut entry_count = entries.len();
    let mut total_bytes = entries
        .iter()
        .map(|(_, bytes, _)| *bytes)
        .fold(0_u64, u64::saturating_add);
    for (path, bytes, _) in entries {
        if entry_count <= max_entries && total_bytes <= max_bytes {
            break;
        }
        if protected == Some(path.as_path()) {
            continue;
        }
        if fs::remove_file(path).is_ok() {
            entry_count = entry_count.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(bytes);
            summary.entries += 1;
        }
    }
    summary.usage = PersistentCacheUsage {
        entries: entry_count,
        bytes: total_bytes,
    };
    summary
}

fn stale_cache_temp_cutoff() -> SystemTime {
    SystemTime::now()
        .checked_sub(STALE_CACHE_TEMP_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn existing_cache_file_len(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| metadata.len())
}

fn is_cache_temporary_file(file_name: Option<&OsStr>) -> bool {
    let Some(file_name) = file_name.and_then(OsStr::to_str) else {
        return false;
    };
    let parts = file_name.split('.').collect::<Vec<_>>();
    parts.len() == 6
        && parts[0].is_empty()
        && parts[1].len() == 16
        && parts[1].bytes().all(|byte| byte.is_ascii_hexdigit())
        && parts[2] == "json"
        && parts[3].parse::<u32>().is_ok()
        && parts[4].parse::<u64>().is_ok()
        && parts[5] == "tmp"
}

fn persistent_entry_path(cache_root: &Path, key: &CacheKey, source_path: &Path) -> PathBuf {
    persistent_namespace_path(cache_root, key)
        .join(format!("{:016x}.json", stable_path_hash(source_path)))
}

fn persistent_namespace_path(cache_root: &Path, key: &CacheKey) -> PathBuf {
    cache_root
        .join(ROLLOUT_CACHE_DIRECTORY)
        .join(cache_namespace(key))
}

fn cache_namespace(key: &CacheKey) -> String {
    let home = key.codex_home.to_string_lossy();
    let redaction = if key.redact_content {
        b"redacted".as_slice()
    } else {
        b"visible".as_slice()
    };
    format!(
        "{:016x}-{}",
        stable_hash(&[home.as_bytes(), redaction]),
        if key.redact_content { "r" } else { "v" }
    )
}

fn stable_path_hash(path: &Path) -> u64 {
    let path = path.to_string_lossy();
    stable_hash(&[path.as_bytes()])
}

fn stable_hash(parts: &[&[u8]]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn reduce_cached_files(
    files: &[RolloutFile],
    cache: &HashMap<PathBuf, CachedFile>,
    config: &CollectConfig,
) -> ReducedRollouts {
    let mut reduced = ReducedRollouts::default();
    for file in files {
        let Some(cached) = cache.get(&file.path) else {
            continue;
        };
        let parsed = &cached.parsed;
        if archived_rollout_is_subsumed_by_active(file, parsed, files, cache, config) {
            continue;
        }
        replay_file_into_reduced(&mut reduced, file, parsed, config);
    }
    rebuild_reduced_metadata(&mut reduced, files, cache);
    reduced
}

fn mark_threads_with_changed_file_order(
    previous: &[SelectedFile],
    current: &[SelectedFile],
    cache: &HashMap<PathBuf, CachedFile>,
    changed_thread_ids: &mut HashSet<String>,
) {
    let paths_by_thread = |selected: &[SelectedFile]| {
        let mut paths = HashMap::<String, Vec<PathBuf>>::new();
        for file in selected {
            let Some(thread_id) = cache
                .get(&file.path)
                .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
            else {
                continue;
            };
            paths
                .entry(thread_id.clone())
                .or_default()
                .push(file.path.clone());
        }
        paths
    };
    let previous_paths = paths_by_thread(previous);
    let current_paths = paths_by_thread(current);

    for thread_id in previous_paths.keys().chain(current_paths.keys()) {
        if previous_paths.get(thread_id) != current_paths.get(thread_id) {
            changed_thread_ids.insert(thread_id.clone());
        }
    }
}

fn incrementally_reduce_cached_files(
    reduced: &mut ReducedRollouts,
    changed_thread_ids: &HashSet<String>,
    files: &[RolloutFile],
    cache: &HashMap<PathBuf, CachedFile>,
    config: &CollectConfig,
) {
    for thread_id in changed_thread_ids {
        reduced.threads.remove(thread_id);
    }
    reduced
        .dataset
        .calls
        .retain(|call| !changed_thread_ids.contains(&call.thread_id));
    reduced
        .dataset
        .rate_observations
        .retain(|observation| !changed_thread_ids.contains(&observation.thread_id));

    let selected_paths = files
        .iter()
        .map(|file| file.path.as_path())
        .collect::<HashSet<_>>();
    reduced.replay_warnings.retain(|path, _| {
        selected_paths.contains(path.as_path())
            && cache
                .get(path)
                .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
                .is_none_or(|thread_id| !changed_thread_ids.contains(thread_id))
    });
    reduced.ambiguous_token_resets.retain(|path, _| {
        selected_paths.contains(path.as_path())
            && cache
                .get(path)
                .and_then(|cached| cached.parsed.owner_thread_id.as_ref())
                .is_none_or(|thread_id| !changed_thread_ids.contains(thread_id))
    });

    for file in files {
        let Some(cached) = cache.get(&file.path) else {
            continue;
        };
        let parsed = &cached.parsed;
        if !parsed
            .owner_thread_id
            .as_ref()
            .is_some_and(|thread_id| changed_thread_ids.contains(thread_id))
            || archived_rollout_is_subsumed_by_active(file, parsed, files, cache, config)
        {
            continue;
        }
        replay_file_into_reduced(reduced, file, parsed, config);
    }
    rebuild_reduced_metadata(reduced, files, cache);
}

fn replay_file_into_reduced(
    reduced: &mut ReducedRollouts,
    file: &RolloutFile,
    parsed: &ParsedFile,
    config: &CollectConfig,
) {
    let mut replay = RolloutDataset::default();
    replay_rollout_file(
        &file.path,
        parsed,
        config,
        &mut reduced.threads,
        &mut replay,
    );
    reduced.dataset.calls.extend(replay.calls);
    reduced
        .dataset
        .rate_observations
        .extend(replay.rate_observations);
    if !replay.warnings.is_empty() {
        reduced
            .replay_warnings
            .insert(file.path.clone(), replay.warnings);
    }
    if replay.stats.ambiguous_token_resets > 0 {
        reduced
            .ambiguous_token_resets
            .insert(file.path.clone(), replay.stats.ambiguous_token_resets);
    }
}

fn rebuild_reduced_metadata(
    reduced: &mut ReducedRollouts,
    files: &[RolloutFile],
    cache: &HashMap<PathBuf, CachedFile>,
) {
    reduced.dataset.stats = Default::default();
    reduced.dataset.warnings.clear();
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
        if let Some(warnings) = reduced.replay_warnings.get(&file.path) {
            reduced.dataset.warnings.extend(warnings.clone());
        }
        reduced.dataset.stats.ambiguous_token_resets += reduced
            .ambiguous_token_resets
            .get(&file.path)
            .copied()
            .unwrap_or_default();
    }
}

fn archived_rollout_is_subsumed_by_active(
    file: &RolloutFile,
    parsed: &ParsedFile,
    files: &[RolloutFile],
    cache: &HashMap<PathBuf, CachedFile>,
    config: &CollectConfig,
) -> bool {
    if !parsed.complete
        || !file
            .path
            .starts_with(config.codex_home.join("archived_sessions"))
    {
        return false;
    }
    let Some(owner_thread_id) = parsed.owner_thread_id.as_deref() else {
        return false;
    };
    let active_root = config.codex_home.join("sessions");

    files.iter().any(|candidate| {
        candidate.path.starts_with(&active_root)
            && cache.get(&candidate.path).is_some_and(|candidate| {
                candidate.parsed.complete
                    && candidate.parsed.owner_thread_id.as_deref() == Some(owner_thread_id)
                    && candidate.parsed.events.starts_with(&parsed.events)
            })
    })
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
    finish_dataset(config, now, &reduced.threads, thread_titles, &mut discovery);
    discovery
}

fn crossed_freshness_deadline(
    reduced: &ReducedRollouts,
    last_materialized_at: DateTime<Utc>,
    now: DateTime<Utc>,
    active_grace: Duration,
) -> bool {
    let Ok(active_grace) = ChronoDuration::from_std(active_grace) else {
        return false;
    };
    reduced.threads.values().any(|thread| {
        !thread.active_turn_ids.is_empty()
            && thread.updated_at.is_some_and(|updated_at| {
                updated_at
                    .checked_add_signed(active_grace)
                    .is_some_and(|deadline| deadline >= last_materialized_at && deadline < now)
            })
    })
}

fn discover_rollout_inventory(
    config: &CollectConfig,
    now: DateTime<Utc>,
    key: &DiscoveryKey,
    refresh: &mut RolloutCacheRefresh,
) -> DiscoveryCache {
    let roots = [
        config.codex_home.join("sessions"),
        config.codex_home.join("archived_sessions"),
    ];
    let mut inventory = HashMap::new();
    let mut directories = HashMap::new();
    let mut root_fingerprints = Vec::with_capacity(roots.len());
    let mut warnings = Vec::new();
    let mut unreadable_files = 0_usize;
    let mut complete = true;
    let mut found_root = false;

    for root in &roots {
        match inspect_rollout_directory(root) {
            Ok(Some(fingerprint)) => {
                found_root = true;
                root_fingerprints.push((root.clone(), Some(fingerprint)));
            }
            Ok(None) => root_fingerprints.push((root.clone(), None)),
            Err(error) => {
                complete = false;
                unreadable_files += 1;
                warnings.push(format!(
                    "could not inspect rollout directory {}: {error}",
                    root.display()
                ));
                root_fingerprints.push((root.clone(), None));
            }
        }
    }

    if !found_root {
        unreadable_files += 1;
        warnings.push(format!(
            "no Codex rollout directories found under {}",
            config.codex_home.display()
        ));
    }

    for (root, fingerprint) in &root_fingerprints {
        if fingerprint.is_none() {
            continue;
        }

        for entry in WalkDir::new(root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    complete = false;
                    unreadable_files += 1;
                    warnings.push(format!(
                        "could not inspect rollout path under {}: {error}",
                        root.display()
                    ));
                    continue;
                }
            };

            if entry.file_type().is_dir() {
                match entry.metadata() {
                    Ok(metadata) => match FileFingerprint::from_metadata(&metadata) {
                        Ok(fingerprint) => {
                            refresh.discovery_probed_dirs += 1;
                            directories.insert(entry.path().to_owned(), fingerprint);
                        }
                        Err(error) => {
                            complete = false;
                            unreadable_files += 1;
                            warnings.push(format!(
                                "could not fingerprint rollout directory {}: {error}",
                                entry.path().display()
                            ));
                        }
                    },
                    Err(error) => {
                        complete = false;
                        unreadable_files += 1;
                        warnings.push(format!(
                            "could not fingerprint rollout directory {}: {error}",
                            entry.path().display()
                        ));
                    }
                }
                continue;
            }
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }

            refresh.discovery_probed_files += 1;
            match inspect_rollout_file(entry.path()) {
                Ok(file) => {
                    inventory.insert(file.path.clone(), file);
                }
                Err(error) => {
                    complete = false;
                    unreadable_files += 1;
                    warnings.push(format!(
                        "could not fingerprint {}: {error}",
                        entry.path().display()
                    ));
                }
            }
        }
    }

    // A directory changing while it is walked can otherwise make a new file
    // invisible until the periodic full rescan. Mark this inventory incomplete
    // so the next refresh retries the complete walk.
    for (path, fingerprint) in &directories {
        refresh.discovery_probed_dirs += 1;
        if inspect_rollout_directory(path).ok().flatten().as_ref() != Some(fingerprint) {
            complete = false;
        }
    }

    DiscoveryCache {
        key: key.clone(),
        inventory,
        directories,
        roots: root_fingerprints,
        warnings,
        unreadable_files,
        complete,
        full_scan_at: Instant::now(),
        logical_now: now,
    }
}

fn refresh_cached_discovery(
    cache: &mut DiscoveryCache,
    selected: &[SelectedFile],
    now: DateTime<Utc>,
    refresh: &mut RolloutCacheRefresh,
) -> Result<(), ()> {
    for (path, expected) in &cache.roots {
        refresh.discovery_probed_dirs += 1;
        let current = inspect_rollout_directory(path).map_err(|_| ())?;
        if current.as_ref() != expected.as_ref() {
            return Err(());
        }
    }
    for (path, expected) in &cache.directories {
        refresh.discovery_probed_dirs += 1;
        let current = inspect_rollout_directory(path).map_err(|_| ())?;
        if current.as_ref() != Some(expected) {
            return Err(());
        }
    }
    refresh_selected_rollout_files(cache, selected, now, refresh)
}

fn refresh_selected_rollout_files(
    cache: &mut DiscoveryCache,
    selected: &[SelectedFile],
    now: DateTime<Utc>,
    refresh: &mut RolloutCacheRefresh,
) -> Result<(), ()> {
    for selected in selected {
        refresh.discovery_probed_files += 1;
        let current = inspect_rollout_file(&selected.path).map_err(|_| ())?;
        cache.inventory.insert(current.path.clone(), current);
    }
    cache.logical_now = now;
    Ok(())
}

fn inspect_rollout_directory(path: &Path) -> std::io::Result<Option<FileFingerprint>> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    FileFingerprint::from_metadata(&metadata).map(Some)
}

fn select_rollout_files(
    cache: &DiscoveryCache,
    config: &CollectConfig,
    now: DateTime<Utc>,
    dataset: &mut RolloutDataset,
) -> Vec<RolloutFile> {
    let lookback_days = config.lookback_days.max(0);
    let cutoff = ChronoDuration::try_days(lookback_days)
        .and_then(|lookback| now.checked_sub_signed(lookback))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    dataset.stats.unreadable_files = cache.unreadable_files;
    dataset.warnings = cache.warnings.clone();
    let mut files = cache
        .inventory
        .values()
        .filter(|file| file.modified_at >= cutoff)
        .cloned()
        .collect::<Vec<_>>();
    dataset.stats.discovered_files = files.len();
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

struct StableParse {
    cached: CachedFile,
    stability_retries: usize,
    tail_parsed: bool,
    parsed_bytes: u64,
}

#[cfg(test)]
fn parse_stable_rollout_file(file: &RolloutFile, config: &CollectConfig) -> (CachedFile, usize) {
    let parsed = refresh_stable_rollout_file(file, config, None);
    (parsed.cached, parsed.stability_retries)
}

fn refresh_stable_rollout_file(
    file: &RolloutFile,
    config: &CollectConfig,
    previous: Option<CachedFile>,
) -> StableParse {
    let mut candidate = file.clone();
    let mut previous = previous;
    let mut parsed_bytes = 0_u64;
    for attempt in 0..2 {
        let (mut parsed, tail_parsed, bytes_read) =
            parse_rollout_candidate(&candidate, config, previous.take());
        parsed_bytes = parsed_bytes.saturating_add(bytes_read);
        let after = match inspect_rollout_file(&candidate.path) {
            Ok(after) => after,
            Err(error) => {
                parsed.complete = false;
                parsed.unreadable_files += 1;
                parsed.warnings.push(format!(
                    "could not inspect {} after reading: {error}",
                    candidate.path.display()
                ));
                return StableParse {
                    cached: CachedFile {
                        fingerprint: candidate.fingerprint,
                        parsed,
                    },
                    stability_retries: attempt,
                    tail_parsed: false,
                    parsed_bytes,
                };
            }
        };

        if after.fingerprint == candidate.fingerprint {
            if parsed.complete
                && let Err(error) =
                    update_tail_guard(&candidate.path, &mut parsed, after.fingerprint.len)
            {
                parsed.complete = false;
                parsed.unreadable_files += 1;
                parsed.warnings.push(format!(
                    "could not fingerprint parsed tail for {}: {error}",
                    candidate.path.display()
                ));
            }
            let final_fingerprint = inspect_rollout_file(&candidate.path)
                .map(|final_file| final_file.fingerprint)
                .unwrap_or_else(|_| after.fingerprint.clone());
            if final_fingerprint == after.fingerprint {
                return StableParse {
                    cached: CachedFile {
                        fingerprint: after.fingerprint,
                        parsed,
                    },
                    stability_retries: attempt,
                    tail_parsed,
                    parsed_bytes,
                };
            }
            if attempt == 0 {
                candidate = RolloutFile {
                    path: candidate.path,
                    modified_at: DateTime::<Utc>::from(final_fingerprint.modified),
                    fingerprint: final_fingerprint,
                };
                continue;
            }
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
        return StableParse {
            cached: CachedFile {
                fingerprint: after.fingerprint,
                parsed,
            },
            stability_retries: attempt,
            tail_parsed: false,
            parsed_bytes,
        };
    }
    unreachable!("stable rollout parsing attempts are bounded")
}

fn parse_rollout_candidate(
    file: &RolloutFile,
    config: &CollectConfig,
    previous: Option<CachedFile>,
) -> (ParsedFile, bool, u64) {
    if let Some(previous) = previous {
        let previous_len = previous.fingerprint.len;
        if can_tail_parse(&previous, file)
            && let Some(parsed) = parse_rollout_tail(file, config, previous)
        {
            return (
                parsed,
                true,
                file.fingerprint.len.saturating_sub(previous_len),
            );
        }
    }
    (
        parse_rollout_file(file, config),
        false,
        file.fingerprint.len,
    )
}

fn can_tail_parse(previous: &CachedFile, file: &RolloutFile) -> bool {
    previous.parsed.complete
        && previous.parsed.ends_with_newline
        && previous.fingerprint.len < file.fingerprint.len
        && previous.fingerprint.same_source_identity(&file.fingerprint)
        && (previous.fingerprint.len == 0 || previous.parsed.tail_guard.len > 0)
}

fn parse_rollout_tail(
    file: &RolloutFile,
    config: &CollectConfig,
    mut previous: CachedFile,
) -> Option<ParsedFile> {
    let mut handle = match File::open(&file.path) {
        Ok(handle) => handle,
        Err(_) => return None,
    };
    if !tail_guard_matches(
        &mut handle,
        previous.fingerprint.len,
        previous.parsed.tail_guard,
    ) {
        return None;
    }
    if handle
        .seek(SeekFrom::Start(previous.fingerprint.len))
        .is_err()
    {
        return None;
    }
    previous.parsed.complete = false;
    Some(parse_rollout_reader(
        file,
        config,
        previous.parsed,
        BufReader::new(handle),
    ))
}

fn tail_guard_matches(handle: &mut File, prefix_len: u64, expected: TailGuard) -> bool {
    let guard_len = usize::from(expected.len);
    if prefix_len == 0 {
        return guard_len == 0;
    }
    if guard_len == 0 || u64::from(expected.len) > prefix_len {
        return false;
    }
    if handle
        .seek(SeekFrom::Start(prefix_len - u64::from(expected.len)))
        .is_err()
    {
        return false;
    }
    let mut bytes = vec![0_u8; guard_len];
    handle.read_exact(&mut bytes).is_ok() && stable_hash(&[&bytes]) == expected.hash
}

fn update_tail_guard(path: &Path, parsed: &mut ParsedFile, file_len: u64) -> std::io::Result<()> {
    if file_len == 0 {
        parsed.ends_with_newline = false;
        parsed.tail_guard = TailGuard::default();
        return Ok(());
    }
    let guard_len = usize::try_from(file_len.min(TAIL_GUARD_BYTES as u64)).unwrap_or(0);
    let mut handle = File::open(path)?;
    handle.seek(SeekFrom::Start(file_len - guard_len as u64))?;
    let mut bytes = vec![0_u8; guard_len];
    handle.read_exact(&mut bytes)?;
    parsed.ends_with_newline = bytes.last() == Some(&b'\n');
    parsed.tail_guard = TailGuard {
        len: guard_len as u16,
        hash: stable_hash(&[&bytes]),
    };
    Ok(())
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
    let handle = match File::open(&file.path) {
        Ok(handle) => handle,
        Err(error) => {
            let mut parsed = ParsedFile::default();
            parsed.unreadable_files += 1;
            parsed
                .warnings
                .push(format!("could not open {}: {error}", file.path.display()));
            return parsed;
        }
    };

    parse_rollout_reader(file, config, ParsedFile::default(), BufReader::new(handle))
}

fn parse_rollout_reader<R: BufRead>(
    file: &RolloutFile,
    config: &CollectConfig,
    mut parsed: ParsedFile,
    reader: R,
) -> ParsedFile {
    let mut owning_thread_id = parsed.owner_thread_id.clone();
    let mut owning_created_at = parsed.owning_created_at;
    let mut replaying_foreign_history = parsed.replaying_foreign_history;

    for line in reader.lines() {
        parsed.source_lines = parsed.source_lines.saturating_add(1);
        let line_number = parsed.source_lines;
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
                                "parent_thread_id",
                                "parentThreadId",
                                "forked_from_id",
                                "forkedFromId",
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
                                "parent_thread_id",
                                "parentThreadId",
                                "forked_from_id",
                                "forkedFromId",
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
                    && retain_latest_foreign_baseline(&mut parsed.events, total_usage)
                {
                    parsed.foreign_baseline_events += 1;
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
                                .and_then(user_authored_input_text)
                                .and_then(title_preview)
                        {
                            parsed.events.push(ParsedEvent::UserMessage {
                                preview: title,
                                turn_id: string_field_in(payload, &["turn_id", "turnId"])
                                    .map(str::to_owned),
                                source: UserMessageSource::EventMessage,
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
                    } else {
                        cache_event_message(&mut parsed.events, payload, timestamp, line_number);
                    }
                }
            }
            Some("response_item") => {
                if !config.redact_content
                    && let Some(payload) = object_at(&record, &["payload"])
                    && let Some((preview, turn_id)) = response_item_user_message(payload)
                {
                    parsed.events.push(ParsedEvent::UserMessage {
                        preview,
                        turn_id,
                        source: UserMessageSource::ResponseItem,
                    });
                }
            }
            _ => {}
        }
    }
    parsed.owning_created_at = owning_created_at;
    parsed.replaying_foreign_history = replaying_foreign_history;
    parsed.complete = true;
    parsed
}

fn retain_latest_foreign_baseline(events: &mut Vec<ParsedEvent>, total_usage: TokenUsage) -> bool {
    if let Some(ParsedEvent::ForeignCounterBaseline(previous)) = events.last_mut() {
        *previous = total_usage;
        false
    } else {
        events.push(ParsedEvent::ForeignCounterBaseline(total_usage));
        true
    }
}

fn cache_event_message(
    events: &mut Vec<ParsedEvent>,
    payload: &Map<String, Value>,
    timestamp: DateTime<Utc>,
    line_number: usize,
) {
    let turn_id = || string_field_in(payload, &["turn_id", "turnId"]).map(str::to_owned);
    match string_field_in(payload, &["type"]) {
        Some("task_started") => {
            let Some(turn_id) = turn_id() else {
                return;
            };
            events.push(ParsedEvent::TaskStarted {
                timestamp,
                turn_id,
                started_at: payload
                    .get("started_at")
                    .or_else(|| payload.get("startedAt"))
                    .and_then(parse_timestamp),
            });
        }
        Some("task_complete") => {
            let Some(turn_id) = turn_id() else {
                return;
            };
            events.push(ParsedEvent::TaskComplete {
                timestamp,
                turn_id,
                completed_at: payload
                    .get("completed_at")
                    .or_else(|| payload.get("completedAt"))
                    .and_then(parse_timestamp),
                duration_ms: u64_field_in(payload, &["duration_ms", "durationMs"]),
            });
        }
        Some("turn_aborted") => {
            let Some(turn_id) = turn_id() else {
                return;
            };
            let reason = string_field_in(payload, &["reason"]).unwrap_or_default();
            events.push(ParsedEvent::TurnAborted {
                timestamp,
                turn_id,
                completed_at: payload
                    .get("completed_at")
                    .or_else(|| payload.get("completedAt"))
                    .and_then(parse_timestamp),
                duration_ms: u64_field_in(payload, &["duration_ms", "durationMs"]),
                failed: reason.contains("fail") || reason.contains("error"),
            });
        }
        Some("token_count") => {
            let total_usage = total_token_usage(payload);
            let last_usage = last_token_usage(payload);
            let rate_limits = payload
                .get("rate_limits")
                .or_else(|| payload.get("rateLimits"))
                .and_then(Value::as_object)
                .and_then(parse_cached_rate_limits);
            if total_usage.is_some() || last_usage.is_some() || rate_limits.is_some() {
                events.push(ParsedEvent::TokenCount {
                    timestamp,
                    line_number,
                    total_usage,
                    last_usage,
                    rate_limits,
                });
            }
        }
        _ => {}
    }
}

fn response_item_user_message(payload: &Map<String, Value>) -> Option<(String, Option<String>)> {
    if string_field_in(payload, &["type"]) != Some("message")
        || string_field_in(payload, &["role"]) != Some("user")
    {
        return None;
    }

    let message = payload
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_object)
        .filter(|item| string_field_in(item, &["type"]) == Some("input_text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter_map(user_authored_input_text)
        .collect::<Vec<_>>()
        .join(" ");
    let preview = title_preview(&message)?;
    let turn_id = string_field_in(payload, &["turn_id", "turnId"])
        .or_else(|| {
            payload
                .get("internal_chat_message_metadata_passthrough")
                .or_else(|| payload.get("internalChatMessageMetadataPassthrough"))
                .and_then(Value::as_object)
                .and_then(|metadata| string_field_in(metadata, &["turn_id", "turnId"]))
        })
        .map(str::to_owned);
    Some((preview, turn_id))
}

fn user_authored_input_text(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || is_synthetic_user_context(value) {
        return None;
    }

    const REQUEST_MARKERS: [&str; 4] = [
        "## My request:",
        "# My request:",
        "## My request for Codex:",
        "# My request for Codex:",
    ];
    let request = REQUEST_MARKERS
        .into_iter()
        .filter_map(|marker| value.rfind(marker).map(|index| (index, marker)))
        .max_by_key(|(index, _)| *index)
        .map(|(index, marker)| value[index + marker.len()..].trim())
        .filter(|request| !request.is_empty());
    request.or(Some(value))
}

fn is_synthetic_user_context(value: &str) -> bool {
    value.starts_with("# AGENTS.md instructions for ")
        || value.starts_with("<environment_context>")
        || value.starts_with("<codex_internal_context")
        || value.starts_with("<turn_aborted>")
        || value.starts_with("<recommended_plugins>")
}

fn is_forked_session(payload: &Map<String, Value>) -> bool {
    [
        "parent_thread_id",
        "parentThreadId",
        "forked_from_id",
        "forkedFromId",
    ]
    .into_iter()
    .filter_map(|field| payload.get(field).and_then(Value::as_str))
    .any(|parent| !parent.trim().is_empty())
        || is_subagent_session(payload)
}

fn session_parent_thread_id<'a>(
    payload: &'a Map<String, Value>,
    owner_thread_id: Option<&str>,
) -> Option<(&'a str, ParentThreadRank)> {
    if !is_subagent_session(payload) {
        return None;
    }

    nested_subagent_parent_thread_id(payload, owner_thread_id)
        .map(|parent| (parent, ParentThreadRank::Nested))
        .or_else(|| {
            valid_parent_thread_id_in(
                payload,
                &["parent_thread_id", "parentThreadId"],
                owner_thread_id,
            )
            .map(|parent| (parent, ParentThreadRank::Direct))
        })
        .or_else(|| {
            valid_parent_thread_id_in(
                payload,
                &["forked_from_id", "forkedFromId"],
                owner_thread_id,
            )
            .map(|parent| (parent, ParentThreadRank::Fork))
        })
}

fn is_subagent_session(payload: &Map<String, Value>) -> bool {
    string_field_in(payload, &["thread_source", "threadSource"])
        .is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
        || payload
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|source| {
                source.contains_key("subagent") || source.contains_key("subAgent")
            })
}

fn nested_subagent_parent_thread_id<'a>(
    payload: &'a Map<String, Value>,
    owner_thread_id: Option<&str>,
) -> Option<&'a str> {
    let source = payload.get("source")?.as_object()?;
    let subagent = source
        .get("subagent")
        .or_else(|| source.get("subAgent"))?
        .as_object()?;
    let thread_spawn = subagent
        .get("thread_spawn")
        .or_else(|| subagent.get("threadSpawn"))?
        .as_object()?;
    valid_parent_thread_id_in(
        thread_spawn,
        &["parent_thread_id", "parentThreadId"],
        owner_thread_id,
    )
}

fn valid_parent_thread_id_in<'a>(
    value: &'a Map<String, Value>,
    fields: &[&str],
    owner_thread_id: Option<&str>,
) -> Option<&'a str> {
    fields.iter().find_map(|field| {
        valid_parent_thread_id(value.get(*field).and_then(Value::as_str), owner_thread_id)
    })
}

fn valid_parent_thread_id<'a>(
    candidate: Option<&'a str>,
    owner_thread_id: Option<&str>,
) -> Option<&'a str> {
    candidate
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .filter(|candidate| owner_thread_id != Some(*candidate))
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
    if path.starts_with(config.codex_home.join("archived_sessions")) {
        thread.seen_archived_file = true;
    } else {
        thread.seen_active_file = true;
    }

    for event in &parsed.events {
        match event {
            ParsedEvent::SessionMeta { timestamp, payload } => {
                apply_session_meta(thread, payload, *timestamp);
            }
            ParsedEvent::ForeignCounterBaseline(total_usage) => {
                thread.previous_cumulative = Some(*total_usage);
            }
            ParsedEvent::UserMessage {
                preview,
                turn_id,
                source,
            } => {
                apply_user_message(thread, preview, turn_id.as_deref(), *source);
            }
            ParsedEvent::ThreadSettingsApplied { service_tier } => {
                thread.service_tier = Some(service_tier.clone());
            }
            ParsedEvent::TurnContext { timestamp, payload } => {
                apply_turn_context(thread, payload, *timestamp);
            }
            ParsedEvent::TaskStarted {
                timestamp,
                turn_id,
                started_at,
            } => activate_turn(thread, turn_id, started_at.unwrap_or(*timestamp)),
            ParsedEvent::TaskComplete {
                timestamp,
                turn_id,
                completed_at,
                duration_ms,
            } => finish_turn(
                thread,
                turn_id,
                completed_at.unwrap_or(*timestamp),
                *duration_ms,
                TurnStatus::Completed,
            ),
            ParsedEvent::TurnAborted {
                timestamp,
                turn_id,
                completed_at,
                duration_ms,
                failed,
            } => finish_turn(
                thread,
                turn_id,
                completed_at.unwrap_or(*timestamp),
                *duration_ms,
                if *failed {
                    TurnStatus::Failed
                } else {
                    TurnStatus::Interrupted
                },
            ),
            ParsedEvent::TokenCount {
                timestamp,
                line_number,
                total_usage,
                last_usage,
                rate_limits,
            } => apply_token_count(
                thread,
                TokenCounterSample {
                    total: *total_usage,
                    last: *last_usage,
                },
                rate_limits.as_ref(),
                *timestamp,
                path,
                *line_number,
                dataset,
            ),
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
    if let Some((parent_thread_id, rank)) =
        session_parent_thread_id(payload, Some(&thread.thread_id))
    {
        let should_replace = match thread.parent_thread_rank {
            None => true,
            Some(current_rank) => rank > current_rank,
        };
        if should_replace {
            thread.parent_thread_id = Some(parent_thread_id.to_owned());
            thread.parent_thread_rank = Some(rank);
        }
    }
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

fn apply_user_message(
    thread: &mut ThreadBuilder,
    message: &str,
    explicit_turn_id: Option<&str>,
    source: UserMessageSource,
) {
    let preview = shorten_preview(message, TURN_MESSAGE_PREVIEW_CHARS);
    let turn_id = explicit_turn_id
        .map(str::to_owned)
        .or_else(|| {
            (source == UserMessageSource::EventMessage)
                .then(|| response_item_turn_with_preview(thread, &preview))
                .flatten()
        })
        .or_else(|| thread.active_turn_ids.last().cloned());
    let replace_title = thread.title.is_none()
        || (thread.title_turn_id == turn_id
            && thread.title_source.is_some_and(|current| source > current));
    if replace_title {
        thread.title = Some(message.to_owned());
        thread.title_source = Some(source);
        thread.title_turn_id = turn_id.clone();
    }

    if let Some(turn_id) = turn_id {
        let turn = ensure_turn(thread, &turn_id);
        if turn.message_preview.is_none()
            || turn.message_source.is_some_and(|current| source > current)
        {
            turn.message_preview = Some(preview);
            turn.message_source = Some(source);
        }
    }
}

fn response_item_turn_with_preview(thread: &ThreadBuilder, preview: &str) -> Option<String> {
    thread.active_turn_ids.iter().rev().find_map(|turn_id| {
        thread.turns.get(turn_id).and_then(|turn| {
            (turn.message_source == Some(UserMessageSource::ResponseItem)
                && turn.message_preview.as_deref() == Some(preview))
            .then(|| turn_id.clone())
        })
    })
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
    completed_at: DateTime<Utc>,
    duration_ms: Option<u64>,
    status: TurnStatus,
) {
    let turn = ensure_turn(thread, turn_id);
    turn.completed_at = Some(completed_at);
    turn.status = status;
    turn.duration_ms = duration_ms.or_else(|| {
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
    usage: TokenCounterSample,
    rate_limits: Option<&CachedRateLimits>,
    timestamp: DateTime<Utc>,
    path: &Path,
    line_number: usize,
    dataset: &mut RolloutDataset,
) {
    if let Some(rate_limits) = rate_limits {
        dataset.rate_observations.push(RateObservation {
            timestamp,
            thread_id: thread.thread_id.clone(),
            turn_id: thread
                .active_turn_ids
                .last()
                .cloned()
                .or_else(|| thread.last_turn_id.clone()),
            limit_id: rate_limits.limit_id.clone(),
            primary: rate_limits.primary.clone(),
            secondary: rate_limits.secondary.clone(),
            provenance: Provenance::LocalExact,
        });
    }

    let Some(total_usage) = usage.total else {
        return;
    };

    let delta = match thread.previous_cumulative {
        None => total_usage,
        Some(previous) if previous == total_usage => return,
        Some(_) if usage.last == Some(total_usage) => total_usage,
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
    let request_usage_exact = usage.last.is_some_and(|last| {
        let breakdown_missing =
            last.total_tokens > 0 && last.input_tokens == 0 && last.output_tokens == 0;
        last == delta && !breakdown_missing
    });

    thread.token_usage.add_assign(delta);
    let turn_id = thread
        .active_turn_ids
        .last()
        .cloned()
        .or_else(|| thread.last_turn_id.clone());
    let (model, service_tier) = turn_id
        .as_deref()
        .and_then(|turn_id| thread.turns.get(turn_id))
        .map(|turn| (turn.model.clone(), turn.service_tier.clone()))
        .unwrap_or_default();
    if let Some(turn_id) = turn_id.as_deref() {
        ensure_turn(thread, turn_id).token_usage.add_assign(delta);
    }
    dataset.calls.push(UsageCall {
        timestamp,
        thread_id: thread.thread_id.clone(),
        turn_id,
        model,
        service_tier,
        tokens: delta,
        request_usage_exact,
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

fn last_token_usage(payload: &Map<String, Value>) -> Option<TokenUsage> {
    payload
        .get("info")
        .and_then(Value::as_object)
        .and_then(|info| {
            info.get("last_token_usage")
                .or_else(|| info.get("lastTokenUsage"))
        })
        .or_else(|| payload.get("last_token_usage"))
        .or_else(|| payload.get("lastTokenUsage"))
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

fn parse_cached_rate_limits(value: &Map<String, Value>) -> Option<CachedRateLimits> {
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

    Some(CachedRateLimits {
        limit_id,
        primary,
        secondary,
    })
}

fn parse_limit_window(value: &Value) -> Option<LimitWindow> {
    let value = value.as_object()?;
    let used_percent = f64_field_in(value, &["used_percent", "usedPercent"])?;
    if !used_percent.is_finite() {
        return None;
    }
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
        "cache_write_input_tokens",
        "cacheWriteInputTokens",
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
    let cache_write_input_tokens = u64_field_in(
        value,
        &["cache_write_input_tokens", "cacheWriteInputTokens"],
    )
    .unwrap_or(0);
    let output_tokens = u64_field_in(value, &["output_tokens", "outputTokens"]).unwrap_or(0);
    let reasoning_output_tokens =
        u64_field_in(value, &["reasoning_output_tokens", "reasoningOutputTokens"]).unwrap_or(0);
    let total_tokens = u64_field_in(value, &["total_tokens", "totalTokens"])
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    })
}

fn finish_dataset(
    config: &CollectConfig,
    now: DateTime<Utc>,
    threads: &HashMap<String, ThreadBuilder>,
    thread_titles: &HashMap<String, String>,
    dataset: &mut RolloutDataset,
) {
    for thread in threads.values() {
        let active_is_fresh = !thread.active_turn_ids.is_empty()
            && timestamp_is_fresh(thread.updated_at, now, config.active_grace);

        let (status, status_provenance, status_confidence) = task_status(thread, active_is_fresh);
        let turn_count = thread.turns.len();
        for turn in thread.turns.values() {
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
            let turn_status = if thread
                .active_turn_ids
                .iter()
                .any(|turn_id| turn_id == &turn.turn_id)
            {
                if active_is_fresh {
                    TurnStatus::InProgress
                } else {
                    TurnStatus::Stale
                }
            } else {
                turn.status
            };
            dataset.turns.push(TurnRecord {
                thread_id: thread.thread_id.clone(),
                turn_id: turn.turn_id.clone(),
                model: turn.model.clone(),
                reasoning_effort: turn.reasoning_effort.clone(),
                service_tier: turn.service_tier.clone(),
                message_preview: turn.message_preview.clone(),
                started_at: turn.started_at,
                completed_at: turn.completed_at,
                duration_ms,
                status: turn_status,
                token_usage: turn.token_usage,
                window_token_usage: TokenUsage::default(),
                local_token_share_percent: 0.0,
                estimated_quota_percent: 0.0,
                quota_confidence: Confidence::Unknown,
                api_equivalent_cost: Default::default(),
            });
        }

        let title = if config.redact_content {
            "[redacted]".to_owned()
        } else {
            thread_titles
                .get(&thread.thread_id)
                .cloned()
                .or_else(|| thread.title.clone())
                .unwrap_or_else(|| "Untitled task".to_owned())
        };
        dataset.tasks.push(TaskRecord {
            thread_id: thread.thread_id.clone(),
            parent_thread_id: thread.parent_thread_id.clone(),
            archived: thread.seen_archived_file && !thread.seen_active_file,
            title,
            cwd: thread.cwd.clone(),
            source: thread.source.clone(),
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
            api_equivalent_cost: Default::default(),
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

#[cfg(test)]
mod tests;
