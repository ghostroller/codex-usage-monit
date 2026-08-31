//! Persistent rolling bandwidth budgets for remote synchronization.
//!
//! This ledger is deliberately separate from [`crate::remote_sync_health`].
//! Health is a latest-attempt snapshot, whereas a rolling budget retains
//! bounded timestamped estimates of on-wire network cost, including a
//! conservative fixed SSH-chain overhead rather than payload bytes alone. The
//! wire format never stores SSH aliases, host names, paths, credentials, or
//! raw error text.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;
use crate::remote_sync::{MIN_REMOTE_SYNC_RESPONSE_BYTES, RemoteSyncReport};
use crate::source_identity::NodeId;
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

pub const REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION: u32 = 1;
pub const REMOTE_BANDWIDTH_WINDOW_HOURS: i64 = 24;
pub const REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES: u64 = 150 * 1024 * 1024;
pub const REMOTE_BANDWIDTH_HARD_LIMIT_BYTES: u64 = 250 * 1024 * 1024;
/// Conservative fixed transport cost for one SSH hop and one exchange.
pub const REMOTE_BANDWIDTH_ESTIMATED_BYTES_PER_SSH_HOP: usize = 100 * 1024;
/// ProxyJump is not parsed yet, so each exchange assumes this many hops.
pub const REMOTE_BANDWIDTH_UNKNOWN_EFFECTIVE_HOPS: usize = 3;
/// Fixed estimated network cost added for every validated SSH response.
pub const REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES: usize =
    REMOTE_BANDWIDTH_ESTIMATED_BYTES_PER_SSH_HOP * REMOTE_BANDWIDTH_UNKNOWN_EFFECTIVE_HOPS;
/// One automatic hard-pause probe negotiates exactly the protocol's smallest
/// usable framed response. It can therefore check liveness without admitting
/// an aggregate data page while the normal hard cap is active.
pub const REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES: usize =
    MIN_REMOTE_SYNC_RESPONSE_BYTES;

const BUDGET_DIRECTORY: &str = "remote-bandwidth-v1";
const BUDGET_FILE: &str = "ledger.json";
const BUDGET_LOCK_FILE: &str = "ledger.lock";
const MAX_BUDGET_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LEDGER_ENTRIES: usize = 65_536;
const MAX_ENTRIES_PER_SOURCE: usize = 2_048;
const MAX_HOST_ID_BYTES: usize = 64;
const MAX_BYTES_PER_ENTRY: u64 = 512 * 1024 * 1024;
const ENTRY_ID_BYTES: usize = 16;
const ENTRY_ID_HEX_BYTES: usize = ENTRY_ID_BYTES * 2;
const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns the estimated on-wire byte cost of one completed sync run.
///
/// Every committed page represents one SSH exchange. A zero-page result still
/// consumed one response (for example `BootstrapRestarted`), so it receives
/// one fixed conservative exchange charge. This estimate deliberately does
/// not claim to be an exact interface byte counter.
pub fn estimated_remote_sync_network_bytes(report: &RemoteSyncReport) -> io::Result<usize> {
    let exchanges = report.pages_committed.max(1);
    let overhead = exchanges
        .checked_mul(REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES)
        .ok_or_else(|| invalid_input("remote bandwidth SSH overhead overflows usize"))?;
    report
        .response_bytes
        .checked_add(overhead)
        .ok_or_else(|| invalid_input("remote bandwidth estimated network bytes overflow usize"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteBandwidthTransferKind {
    /// Bootstrap, cursor continuation, or explicit history backfill.
    AutomaticBulk,
    /// Ordinary delta after a completed bootstrap.
    AutomaticIncremental,
    /// Fixed-size liveness probe allowed through a limit-reached hard pause.
    /// Callers cannot construct an arbitrary bypass with this variant; only
    /// `begin_automatic_probe_attempt` admits its exact bounded reservation.
    AutomaticProbe,
    /// Explicit user synchronization; hard cap remains enforced.
    Manual,
    /// Explicit one-invocation override for a future `--ignore-budget` flag.
    ManualOverride,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBandwidthBudgetLevel {
    Soft,
    Hard,
}

/// Bounded taxonomy: no variant carries caller-controlled text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBandwidthPauseReason {
    LimitReached,
    ClockAnomaly,
    LedgerCapacity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBandwidthUsage {
    host_id: String,
    node_id: Option<NodeId>,
    committed_bytes: u64,
    reserved_bytes: u64,
    oldest_expiry_at: Option<DateTime<Utc>>,
}

impl RemoteBandwidthUsage {
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
    pub fn node_id(&self) -> Option<&NodeId> {
        self.node_id.as_ref()
    }
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
    pub fn rolling_bytes(&self) -> u64 {
        self.committed_bytes.saturating_add(self.reserved_bytes)
    }
    pub fn oldest_expiry_at(&self) -> Option<DateTime<Utc>> {
        self.oldest_expiry_at
    }
}

/// A structured fail-closed response. A caller must not open SSH for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBandwidthBudgetPause {
    usage: RemoteBandwidthUsage,
    level: RemoteBandwidthBudgetLevel,
    reason: RemoteBandwidthPauseReason,
    limit_bytes: u64,
    resume_at: Option<DateTime<Utc>>,
}

impl RemoteBandwidthBudgetPause {
    pub fn usage(&self) -> &RemoteBandwidthUsage {
        &self.usage
    }
    pub fn level(&self) -> RemoteBandwidthBudgetLevel {
        self.level
    }
    pub fn reason(&self) -> RemoteBandwidthPauseReason {
        self.reason
    }
    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }
    pub fn resume_at(&self) -> Option<DateTime<Utc>> {
        self.resume_at
    }
    pub fn budget_paused(&self) -> bool {
        true
    }
}

/// Opaque admission token. Apply `granted_response_bytes()` to the bounded
/// sync limits before opening SSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBandwidthReservation {
    id: String,
    host_id: String,
    node_id: Option<NodeId>,
    started_at: DateTime<Utc>,
    reserved_bytes: u64,
    reserved_transport_overhead_bytes: u64,
    kind: RemoteBandwidthTransferKind,
}

impl RemoteBandwidthReservation {
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
    pub fn node_id(&self) -> Option<&NodeId> {
        self.node_id.as_ref()
    }
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
    pub fn granted_response_bytes(&self) -> io::Result<usize> {
        usize::try_from(
            self.reserved_bytes
                .checked_sub(self.reserved_transport_overhead_bytes)
                .ok_or_else(|| invalid_data("remote bandwidth reservation overhead is invalid"))?,
        )
        .map_err(|_| invalid_data("remote bandwidth reservation does not fit platform usize"))
    }

    pub fn kind(&self) -> RemoteBandwidthTransferKind {
        self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteBandwidthAdmission {
    Granted(RemoteBandwidthReservation),
    Paused(RemoteBandwidthBudgetPause),
}

#[derive(Clone, Copy, Debug)]
struct ReservationByteRequest {
    requested_total_bytes: usize,
    reserved_transport_overhead_bytes: usize,
    minimum_grant_bytes: usize,
}

/// Typed inner `io::Error` used by the existing `RemoteSyncError::Local`
/// boundary. This avoids widening that public error enum while still letting
/// the scheduler distinguish a policy pause from a transient local failure.
#[derive(Clone, Debug)]
pub struct RemoteBandwidthBudgetPausedError {
    pause: RemoteBandwidthBudgetPause,
}

impl RemoteBandwidthBudgetPausedError {
    pub fn new(pause: RemoteBandwidthBudgetPause) -> Self {
        Self { pause }
    }

    pub fn pause(&self) -> &RemoteBandwidthBudgetPause {
        &self.pause
    }
}

impl std::fmt::Display for RemoteBandwidthBudgetPausedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "remote bandwidth budget paused ({:?} {:?})",
            self.pause.level, self.pause.reason
        )
    }
}

impl std::error::Error for RemoteBandwidthBudgetPausedError {}

pub fn budget_pause_from_io_error(error: &io::Error) -> Option<&RemoteBandwidthBudgetPause> {
    error
        .get_ref()?
        .downcast_ref::<RemoteBandwidthBudgetPausedError>()
        .map(RemoteBandwidthBudgetPausedError::pause)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LedgerEntryKind {
    Committed,
    Reserved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LedgerEntry {
    id: String,
    host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_id: Option<NodeId>,
    recorded_at: DateTime<Utc>,
    bytes: u64,
    kind: LedgerEntryKind,
}

impl LedgerEntry {
    fn matches_source(&self, host_id: &str, node_id: Option<&NodeId>) -> bool {
        match node_id {
            Some(node_id) => self.node_id.as_ref() == Some(node_id),
            None => self.node_id.is_none() && self.host_id == host_id,
        }
    }

    fn expires_at(&self) -> io::Result<DateTime<Utc>> {
        self.recorded_at
            .checked_add_signed(window_duration())
            .ok_or_else(|| invalid_data("remote bandwidth expiry timestamp overflows"))
    }

    fn validate(&self) -> io::Result<()> {
        validate_stored_host_id(&self.host_id)?;
        validate_entry_id(&self.id)?;
        if self.bytes == 0 || self.bytes > MAX_BYTES_PER_ENTRY {
            return Err(invalid_data(
                "remote bandwidth entry byte count is outside the supported range",
            ));
        }
        let _ = self.expires_at()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteBandwidthLedger {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_observed_at: Option<DateTime<Utc>>,
    entries: Vec<LedgerEntry>,
}

impl Default for StoredRemoteBandwidthLedger {
    fn default() -> Self {
        Self {
            schema_version: REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION,
            last_observed_at: None,
            entries: Vec::new(),
        }
    }
}

impl StoredRemoteBandwidthLedger {
    fn validate(&self) -> io::Result<()> {
        if self.schema_version != REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION {
            let relation = if self.schema_version > REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "{relation} remote bandwidth schema version {}; expected {}",
                self.schema_version, REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION
            )));
        }
        if self.entries.len() > MAX_LEDGER_ENTRIES {
            return Err(invalid_data("remote bandwidth ledger has too many entries"));
        }
        let mut ids = HashSet::with_capacity(self.entries.len());
        let mut source_counts = HashMap::<String, usize>::new();
        let mut previous: Option<(DateTime<Utc>, &str)> = None;
        for entry in &self.entries {
            entry.validate()?;
            if !ids.insert(entry.id.as_str()) {
                return Err(invalid_data(
                    "remote bandwidth ledger contains a duplicate entry ID",
                ));
            }
            let count = source_counts
                .entry(source_storage_key(&entry.host_id, entry.node_id.as_ref()))
                .or_default();
            *count += 1;
            if *count > MAX_ENTRIES_PER_SOURCE {
                return Err(invalid_data(
                    "remote bandwidth ledger has too many entries for one source",
                ));
            }
            if previous.is_some_and(|previous| previous >= (entry.recorded_at, entry.id.as_str())) {
                return Err(invalid_data(
                    "remote bandwidth ledger entries are not strictly sorted",
                ));
            }
            previous = Some((entry.recorded_at, entry.id.as_str()));
        }
        if let (Some(last_observed), Some(last_entry)) =
            (self.last_observed_at, self.entries.last())
            && last_observed < last_entry.recorded_at
        {
            return Err(invalid_data(
                "remote bandwidth last-observed time precedes a ledger entry",
            ));
        }
        Ok(())
    }

    fn sort_entries(&mut self) {
        self.entries
            .sort_by(|a, b| (a.recorded_at, &a.id).cmp(&(b.recorded_at, &b.id)));
    }

    fn clock_anomaly_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.last_observed_at
            .filter(|time| *time > now)
            .into_iter()
            .chain(
                self.entries
                    .iter()
                    .filter(|entry| entry.recorded_at > now)
                    .map(|entry| entry.recorded_at),
            )
            .max()
    }

    fn advance_clock_and_prune(&mut self, now: DateTime<Utc>) -> io::Result<()> {
        if self.clock_anomaly_at(now).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote bandwidth clock moved backwards",
            ));
        }
        let cutoff = now
            .checked_sub_signed(window_duration())
            .ok_or_else(|| invalid_input("remote bandwidth window timestamp underflows"))?;
        self.entries.retain(|entry| entry.recorded_at > cutoff);
        self.last_observed_at = Some(now);
        Ok(())
    }

    fn source_entries<'a>(
        &'a self,
        host_id: &'a str,
        node_id: Option<&'a NodeId>,
    ) -> impl Iterator<Item = &'a LedgerEntry> + 'a {
        self.entries
            .iter()
            .filter(move |entry| entry.matches_source(host_id, node_id))
    }

    fn usage(&self, host_id: &str, node_id: Option<&NodeId>) -> RemoteBandwidthUsage {
        let mut committed_bytes = 0_u64;
        let mut reserved_bytes = 0_u64;
        let mut oldest_expiry_at: Option<DateTime<Utc>> = None;
        for entry in self.source_entries(host_id, node_id) {
            match entry.kind {
                LedgerEntryKind::Committed => {
                    committed_bytes = committed_bytes.saturating_add(entry.bytes)
                }
                LedgerEntryKind::Reserved => {
                    reserved_bytes = reserved_bytes.saturating_add(entry.bytes)
                }
            }
            if let Ok(expiry) = entry.expires_at() {
                oldest_expiry_at =
                    Some(oldest_expiry_at.map_or(expiry, |oldest| oldest.min(expiry)));
            }
        }
        RemoteBandwidthUsage {
            host_id: host_id.to_owned(),
            node_id: node_id.cloned(),
            committed_bytes,
            reserved_bytes,
            oldest_expiry_at,
        }
    }

    fn resume_below(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        threshold: u64,
    ) -> Option<DateTime<Utc>> {
        let mut remaining = self.usage(host_id, node_id).rolling_bytes();
        let mut entries = self.source_entries(host_id, node_id).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.recorded_at);
        let mut index = 0;
        while index < entries.len() {
            let at = entries[index].recorded_at;
            while index < entries.len() && entries[index].recorded_at == at {
                remaining = remaining.saturating_sub(entries[index].bytes);
                index += 1;
            }
            if remaining < threshold {
                return at.checked_add_signed(window_duration());
            }
        }
        None
    }

    fn source_entry_count(&self, host_id: &str, node_id: Option<&NodeId>) -> usize {
        self.source_entries(host_id, node_id).count()
    }

    /// Reduces steady-state file growth while conservatively retaining earlier
    /// bytes for at most 59 seconds.
    fn merge_committed_minute_buckets(&mut self) -> io::Result<()> {
        self.sort_entries();
        let mut compacted: Vec<LedgerEntry> = Vec::with_capacity(self.entries.len());
        let mut committed_groups = HashMap::<(String, i64), Vec<usize>>::new();
        for entry in self.entries.drain(..) {
            if entry.kind == LedgerEntryKind::Reserved {
                // Reservations are independently cancellable/settleable by
                // ID, so even same-source same-minute reservations must stay
                // byte-for-byte distinct.
                compacted.push(entry);
                continue;
            }
            let group_key = (
                source_storage_key(&entry.host_id, entry.node_id.as_ref()),
                utc_minute_key(entry.recorded_at),
            );
            let previous_index = committed_groups
                .get(&group_key)
                .and_then(|indices| indices.last())
                .copied();
            if let Some(previous_index) = previous_index {
                let combined = compacted[previous_index]
                    .bytes
                    .checked_add(entry.bytes)
                    .ok_or_else(|| invalid_data("remote bandwidth compacted bytes overflow"))?;
                if combined <= MAX_BYTES_PER_ENTRY {
                    let previous = &mut compacted[previous_index];
                    previous.bytes = combined;
                    // Expiring the entire merged charge at the latest event
                    // in the minute can overcount for at most 59 seconds, but
                    // can never drop earlier bytes before their true expiry.
                    previous.recorded_at = previous.recorded_at.max(entry.recorded_at);
                    continue;
                }
            }
            // The per-entry bound is a corruption/memory guard, not a reason
            // to lose an otherwise valid charge. Start another stable bin for
            // this source-minute when the previous bin cannot absorb it.
            let index = compacted.len();
            compacted.push(entry);
            committed_groups.entry(group_key).or_default().push(index);
        }
        self.entries = compacted;
        self.sort_entries();
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RemoteBandwidthBudgetStore {
    state_root: PathBuf,
}

impl RemoteBandwidthBudgetStore {
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Atomically checks policy and reserves budget for one bounded call.
    /// Reservations count against the rolling cap, preventing concurrent
    /// callers from all observing the same free bytes.
    pub fn begin_attempt(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
        kind: RemoteBandwidthTransferKind,
        requested_max_response_bytes: usize,
    ) -> io::Result<RemoteBandwidthAdmission> {
        self.begin_attempt_inner(
            host_id,
            node_id,
            now,
            kind,
            ReservationByteRequest {
                requested_total_bytes: requested_max_response_bytes,
                reserved_transport_overhead_bytes: 0,
                minimum_grant_bytes: 1,
            },
        )
    }

    /// Reserves both the bounded response payload and conservative transport
    /// overhead for a complete synchronization run.
    ///
    /// The returned response grant has the overhead removed. Consequently a
    /// successful report whose page count stays within `max_exchanges` cannot
    /// settle above its reservation merely because SSH overhead was added.
    pub fn begin_sync_attempt(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
        kind: RemoteBandwidthTransferKind,
        requested_max_response_bytes: usize,
        max_exchanges: usize,
    ) -> io::Result<RemoteBandwidthAdmission> {
        if max_exchanges == 0 {
            return Err(invalid_input(
                "remote bandwidth sync reservation requires at least one exchange",
            ));
        }
        let overhead = max_exchanges
            .checked_mul(REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES)
            .ok_or_else(|| {
                invalid_input("remote bandwidth reservation overhead overflows usize")
            })?;
        let requested_total = requested_max_response_bytes
            .checked_add(overhead)
            .ok_or_else(|| invalid_input("remote bandwidth reservation total overflows usize"))?;
        let minimum_total = MIN_REMOTE_SYNC_RESPONSE_BYTES
            .checked_add(overhead)
            .ok_or_else(|| invalid_input("remote bandwidth minimum reservation overflows usize"))?;
        self.begin_attempt_inner(
            host_id,
            node_id,
            now,
            kind,
            ReservationByteRequest {
                requested_total_bytes: requested_total,
                reserved_transport_overhead_bytes: overhead,
                minimum_grant_bytes: minimum_total,
            },
        )
    }

    /// Reserves one minimum-size response plus one conservative SSH-chain
    /// charge for a due hard-pause health probe. Unlike ordinary data
    /// transfers this exact reservation may cross a limit-reached hard cap;
    /// clock anomalies and ledger-capacity pauses still fail closed.
    pub fn begin_automatic_probe_attempt(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
    ) -> io::Result<RemoteBandwidthAdmission> {
        let overhead = REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES;
        let requested_total = REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES
            .checked_add(overhead)
            .ok_or_else(|| invalid_input("remote bandwidth probe reservation overflows usize"))?;
        self.begin_attempt_inner(
            host_id,
            node_id,
            now,
            RemoteBandwidthTransferKind::AutomaticProbe,
            ReservationByteRequest {
                requested_total_bytes: requested_total,
                reserved_transport_overhead_bytes: overhead,
                minimum_grant_bytes: requested_total,
            },
        )
    }

    fn begin_attempt_inner(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
        kind: RemoteBandwidthTransferKind,
        bytes: ReservationByteRequest,
    ) -> io::Result<RemoteBandwidthAdmission> {
        validate_host_id(host_id)?;
        let requested = u64::try_from(bytes.requested_total_bytes)
            .map_err(|_| invalid_input("remote bandwidth request does not fit u64"))?;
        let reserved_transport_overhead_bytes =
            u64::try_from(bytes.reserved_transport_overhead_bytes)
                .map_err(|_| invalid_input("remote bandwidth overhead does not fit u64"))?;
        let minimum_grant = u64::try_from(bytes.minimum_grant_bytes)
            .map_err(|_| invalid_input("remote bandwidth minimum grant does not fit u64"))?;
        if requested == 0
            || requested > MAX_BYTES_PER_ENTRY
            || minimum_grant == 0
            || minimum_grant > requested
            || reserved_transport_overhead_bytes >= minimum_grant
        {
            return Err(invalid_input(
                "remote bandwidth requested bytes are outside the supported range",
            ));
        }
        if kind == RemoteBandwidthTransferKind::AutomaticProbe {
            let expected_overhead = u64::try_from(
                REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES,
            )
            .map_err(|_| invalid_input("remote bandwidth probe overhead does not fit u64"))?;
            let expected_total = u64::try_from(
                REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES
                    .checked_add(REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES)
                    .ok_or_else(|| {
                        invalid_input("remote bandwidth probe reservation overflows usize")
                    })?,
            )
            .map_err(|_| invalid_input("remote bandwidth probe reservation does not fit u64"))?;
            if requested != expected_total
                || minimum_grant != expected_total
                || reserved_transport_overhead_bytes != expected_overhead
            {
                return Err(invalid_input(
                    "automatic remote probe requires the fixed bounded reservation",
                ));
            }
        }
        let node_id = node_id.cloned();
        self.mutate(|ledger| {
            if let Some(resume_at) = ledger.clock_anomaly_at(now) {
                return Ok(RemoteBandwidthAdmission::Paused(pause(
                    ledger.usage(host_id, node_id.as_ref()),
                    RemoteBandwidthBudgetLevel::Hard,
                    RemoteBandwidthPauseReason::ClockAnomaly,
                    REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    Some(resume_at),
                )));
            }
            ledger.advance_clock_and_prune(now)?;
            // Compact before capacity admission as well as after settlement.
            // This lets an older interleaved ledger already sitting at the
            // per-source cap recover immediately after upgrading.
            ledger.merge_committed_minute_buckets()?;
            let usage = ledger.usage(host_id, node_id.as_ref());
            let rolling = usage.rolling_bytes();

            let override_hard = matches!(
                kind,
                RemoteBandwidthTransferKind::ManualOverride
                    | RemoteBandwidthTransferKind::AutomaticProbe
            );
            if !override_hard && rolling >= REMOTE_BANDWIDTH_HARD_LIMIT_BYTES {
                return Ok(RemoteBandwidthAdmission::Paused(pause(
                    usage,
                    RemoteBandwidthBudgetLevel::Hard,
                    RemoteBandwidthPauseReason::LimitReached,
                    REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    ledger.resume_below(
                        host_id,
                        node_id.as_ref(),
                        REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    ),
                )));
            }

            // Hard always wins when both thresholds are exceeded; consumers
            // can therefore surface the actionable severity without inferring
            // it from the byte count.
            if kind == RemoteBandwidthTransferKind::AutomaticBulk
                && rolling >= REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES
            {
                return Ok(RemoteBandwidthAdmission::Paused(pause(
                    usage,
                    RemoteBandwidthBudgetLevel::Soft,
                    RemoteBandwidthPauseReason::LimitReached,
                    REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
                    ledger.resume_below(
                        host_id,
                        node_id.as_ref(),
                        REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
                    ),
                )));
            }

            let global_capacity_reached = ledger.entries.len() >= MAX_LEDGER_ENTRIES;
            let source_capacity_reached =
                ledger.source_entry_count(host_id, node_id.as_ref()) >= MAX_ENTRIES_PER_SOURCE;
            if global_capacity_reached || source_capacity_reached {
                let resume_at = if global_capacity_reached {
                    ledger
                        .entries
                        .iter()
                        .filter_map(|entry| entry.expires_at().ok())
                        .min()
                } else {
                    ledger
                        .source_entries(host_id, node_id.as_ref())
                        .filter_map(|entry| entry.expires_at().ok())
                        .min()
                };
                return Ok(RemoteBandwidthAdmission::Paused(pause(
                    usage,
                    RemoteBandwidthBudgetLevel::Hard,
                    RemoteBandwidthPauseReason::LedgerCapacity,
                    REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    resume_at,
                )));
            }

            let mut granted = if override_hard {
                requested
            } else {
                requested.min(REMOTE_BANDWIDTH_HARD_LIMIT_BYTES.saturating_sub(rolling))
            };
            if kind == RemoteBandwidthTransferKind::AutomaticBulk {
                granted = granted.min(REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES.saturating_sub(rolling));
            }
            if granted < minimum_grant {
                let (level, limit) = if kind == RemoteBandwidthTransferKind::AutomaticBulk {
                    (
                        RemoteBandwidthBudgetLevel::Soft,
                        REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
                    )
                } else {
                    (
                        RemoteBandwidthBudgetLevel::Hard,
                        REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    )
                };
                return Ok(RemoteBandwidthAdmission::Paused(pause(
                    usage,
                    level,
                    RemoteBandwidthPauseReason::LimitReached,
                    limit,
                    ledger.resume_below(host_id, node_id.as_ref(), limit),
                )));
            }

            let id = allocate_entry_id(&ledger.entries)?;
            ledger.entries.push(LedgerEntry {
                id: id.clone(),
                host_id: host_id.to_owned(),
                node_id: node_id.clone(),
                recorded_at: now,
                bytes: granted,
                kind: LedgerEntryKind::Reserved,
            });
            ledger.sort_entries();
            Ok(RemoteBandwidthAdmission::Granted(
                RemoteBandwidthReservation {
                    id,
                    host_id: host_id.to_owned(),
                    node_id,
                    started_at: now,
                    reserved_bytes: granted,
                    reserved_transport_overhead_bytes,
                    kind,
                },
            ))
        })
    }

    /// Replaces a reservation with a caller-supplied explicit total byte cost.
    ///
    /// This low-level method does not add transport overhead. Production sync
    /// reports should use [`Self::complete_report`], while tests or transports
    /// with an exact total can deliberately supply their own complete value.
    /// An unexpectedly larger total is still fully charged so the next
    /// admission fails closed rather than silently undercounting.
    pub fn complete_attempt(
        &self,
        reservation: &RemoteBandwidthReservation,
        completed_at: DateTime<Utc>,
        explicit_total_bytes: usize,
    ) -> io::Result<RemoteBandwidthUsage> {
        let actual = u64::try_from(explicit_total_bytes)
            .map_err(|_| invalid_input("remote bandwidth total does not fit u64"))?;
        if actual > MAX_BYTES_PER_ENTRY {
            return Err(invalid_input(
                "remote bandwidth total exceeds the supported entry bound",
            ));
        }
        self.mutate(|ledger| {
            ensure_completion_clock(ledger, reservation, completed_at)?;
            let index = find_reservation(ledger, reservation)?;
            ledger.entries.remove(index);
            if actual > 0 {
                ledger.entries.push(LedgerEntry {
                    id: reservation.id.clone(),
                    host_id: reservation.host_id.clone(),
                    node_id: reservation.node_id.clone(),
                    recorded_at: completed_at,
                    bytes: actual,
                    kind: LedgerEntryKind::Committed,
                });
            }
            ledger.last_observed_at = Some(
                ledger
                    .last_observed_at
                    .map_or(completed_at, |observed| observed.max(completed_at)),
            );
            ledger.merge_committed_minute_buckets()?;
            Ok(ledger.usage(&reservation.host_id, reservation.node_id.as_ref()))
        })
    }

    /// Settles a normal successful sync with its estimated on-wire network
    /// cost: validated framed response bytes plus conservative per-exchange
    /// SSH-chain overhead.
    pub fn complete_report(
        &self,
        reservation: &RemoteBandwidthReservation,
        completed_at: DateTime<Utc>,
        report: &RemoteSyncReport,
    ) -> io::Result<RemoteBandwidthUsage> {
        self.complete_attempt(
            reservation,
            completed_at,
            estimated_remote_sync_network_bytes(report)?,
        )
    }

    /// Settles a successful fixed-size automatic probe. A failed or ambiguous
    /// transport deliberately leaves its bounded reservation charged until
    /// expiry, matching normal sync crash/failure accounting.
    pub fn complete_automatic_probe_attempt(
        &self,
        reservation: &RemoteBandwidthReservation,
        completed_at: DateTime<Utc>,
        response_bytes: usize,
    ) -> io::Result<RemoteBandwidthUsage> {
        if reservation.kind != RemoteBandwidthTransferKind::AutomaticProbe {
            return Err(invalid_input(
                "remote bandwidth reservation is not an automatic probe",
            ));
        }
        if response_bytes > REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES {
            return Err(invalid_input(
                "automatic remote probe response exceeded its fixed bound",
            ));
        }
        let total = response_bytes
            .checked_add(REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES)
            .ok_or_else(|| invalid_input("remote bandwidth probe total overflows usize"))?;
        self.complete_attempt(reservation, completed_at, total)
    }

    /// Removes a reservation only when no framed response was received. A
    /// process crash intentionally leaves it charged until the window expires.
    pub fn cancel_attempt(
        &self,
        reservation: &RemoteBandwidthReservation,
        cancelled_at: DateTime<Utc>,
    ) -> io::Result<RemoteBandwidthUsage> {
        self.mutate(|ledger| {
            ensure_completion_clock(ledger, reservation, cancelled_at)?;
            let index = find_reservation(ledger, reservation)?;
            ledger.entries.remove(index);
            ledger.last_observed_at = Some(
                ledger
                    .last_observed_at
                    .map_or(cancelled_at, |observed| observed.max(cancelled_at)),
            );
            Ok(ledger.usage(&reservation.host_id, reservation.node_id.as_ref()))
        })
    }

    /// Reads a pruned snapshot under the same exclusive lock as admissions.
    /// A future timestamp or rollback returns a structured hard pause.
    pub fn usage(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
    ) -> io::Result<Result<RemoteBandwidthUsage, RemoteBandwidthBudgetPause>> {
        validate_host_id(host_id)?;
        let node_id = node_id.cloned();
        self.mutate(|ledger| {
            if let Some(resume_at) = ledger.clock_anomaly_at(now) {
                return Ok(Err(pause(
                    ledger.usage(host_id, node_id.as_ref()),
                    RemoteBandwidthBudgetLevel::Hard,
                    RemoteBandwidthPauseReason::ClockAnomaly,
                    REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    Some(resume_at),
                )));
            }
            ledger.advance_clock_and_prune(now)?;
            Ok(Ok(ledger.usage(host_id, node_id.as_ref())))
        })
    }

    /// Checks the current policy without reserving bytes. This is suitable for
    /// local health/status rendering; it performs no network operation. A
    /// later caller must still use `begin_attempt` because this snapshot alone
    /// is not an admission under concurrency.
    pub fn check(
        &self,
        host_id: &str,
        node_id: Option<&NodeId>,
        now: DateTime<Utc>,
        kind: RemoteBandwidthTransferKind,
    ) -> io::Result<Result<RemoteBandwidthUsage, RemoteBandwidthBudgetPause>> {
        validate_host_id(host_id)?;
        let node_id = node_id.cloned();
        self.mutate(|ledger| {
            if let Some(resume_at) = ledger.clock_anomaly_at(now) {
                return Ok(Err(pause(
                    ledger.usage(host_id, node_id.as_ref()),
                    RemoteBandwidthBudgetLevel::Hard,
                    RemoteBandwidthPauseReason::ClockAnomaly,
                    REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                    Some(resume_at),
                )));
            }
            ledger.advance_clock_and_prune(now)?;
            Ok(check_loaded_policy(ledger, host_id, node_id.as_ref(), kind))
        })
    }

    /// Reads policy for several configured sources under one shared lock.
    ///
    /// Unlike [`Self::check`] and [`Self::usage`], this observer never creates
    /// state, advances the persisted clock, prunes the durable ledger, or
    /// atomically rewrites it. Expiry is applied only to the private in-memory
    /// snapshot. A later transfer must still call [`Self::begin_attempt`].
    pub fn check_many_read_only(
        &self,
        sources: &[(&str, Option<&NodeId>)],
        now: DateTime<Utc>,
        kind: RemoteBandwidthTransferKind,
    ) -> io::Result<Vec<Result<RemoteBandwidthUsage, RemoteBandwidthBudgetPause>>> {
        for (host_id, _) in sources {
            validate_host_id(host_id)?;
        }
        let mut ledger = self.read_ledger_snapshot()?;
        if let Some(resume_at) = ledger.clock_anomaly_at(now) {
            return Ok(sources
                .iter()
                .map(|(host_id, node_id)| {
                    Err(pause(
                        ledger.usage(host_id, *node_id),
                        RemoteBandwidthBudgetLevel::Hard,
                        RemoteBandwidthPauseReason::ClockAnomaly,
                        REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
                        Some(resume_at),
                    ))
                })
                .collect());
        }
        // This mutates only the local clone returned by read_ledger_snapshot.
        ledger.advance_clock_and_prune(now)?;
        Ok(sources
            .iter()
            .map(|(host_id, node_id)| check_loaded_policy(&ledger, host_id, *node_id, kind))
            .collect())
    }

    fn read_ledger_snapshot(&self) -> io::Result<StoredRemoteBandwidthLedger> {
        if !self.state_root.is_absolute() {
            return Err(invalid_input(
                "remote bandwidth state root must be absolute",
            ));
        }
        match validate_state_root(&self.state_root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StoredRemoteBandwidthLedger::default());
            }
            Err(error) => return Err(error),
        }
        let directory = self.budget_directory();
        match validate_private_directory(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StoredRemoteBandwidthLedger::default());
            }
            Err(error) => return Err(error),
        }
        let path = self.budget_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                validate_private_file_metadata(&metadata, "remote bandwidth ledger file")?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StoredRemoteBandwidthLedger::default());
            }
            Err(error) => return Err(error),
        }
        let _lock = open_existing_shared_lock_file(&directory)?;
        let contents =
            read_private_bounded(&path, MAX_BUDGET_FILE_BYTES, "remote bandwidth ledger file")?;
        deserialize_ledger(&contents)
    }

    fn budget_directory(&self) -> PathBuf {
        self.state_root.join(BUDGET_DIRECTORY)
    }

    fn budget_path(&self) -> PathBuf {
        self.budget_directory().join(BUDGET_FILE)
    }

    fn mutate<R>(
        &self,
        operation: impl FnOnce(&mut StoredRemoteBandwidthLedger) -> io::Result<R>,
    ) -> io::Result<R> {
        let directory = self.budget_directory();
        create_private_directory_beneath(&self.state_root, &directory)?;
        let _lock = open_locked_lock_file(&directory)?;
        let mut ledger = read_optional_ledger(&self.budget_path())?;
        let result = operation(&mut ledger)?;
        ledger.validate()?;
        write_ledger_atomically(&self.budget_path(), &ledger)?;
        Ok(result)
    }
}

fn check_loaded_policy(
    ledger: &StoredRemoteBandwidthLedger,
    host_id: &str,
    node_id: Option<&NodeId>,
    kind: RemoteBandwidthTransferKind,
) -> Result<RemoteBandwidthUsage, RemoteBandwidthBudgetPause> {
    let usage = ledger.usage(host_id, node_id);
    let rolling = usage.rolling_bytes();
    if !matches!(
        kind,
        RemoteBandwidthTransferKind::ManualOverride | RemoteBandwidthTransferKind::AutomaticProbe
    ) && rolling >= REMOTE_BANDWIDTH_HARD_LIMIT_BYTES
    {
        return Err(pause(
            usage,
            RemoteBandwidthBudgetLevel::Hard,
            RemoteBandwidthPauseReason::LimitReached,
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
            ledger.resume_below(host_id, node_id, REMOTE_BANDWIDTH_HARD_LIMIT_BYTES),
        ));
    }
    if kind == RemoteBandwidthTransferKind::AutomaticBulk
        && rolling >= REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES
    {
        return Err(pause(
            usage,
            RemoteBandwidthBudgetLevel::Soft,
            RemoteBandwidthPauseReason::LimitReached,
            REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
            ledger.resume_below(host_id, node_id, REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES),
        ));
    }
    Ok(usage)
}

fn pause(
    usage: RemoteBandwidthUsage,
    level: RemoteBandwidthBudgetLevel,
    reason: RemoteBandwidthPauseReason,
    limit_bytes: u64,
    resume_at: Option<DateTime<Utc>>,
) -> RemoteBandwidthBudgetPause {
    RemoteBandwidthBudgetPause {
        usage,
        level,
        reason,
        limit_bytes,
        resume_at,
    }
}

fn ensure_completion_clock(
    _ledger: &StoredRemoteBandwidthLedger,
    reservation: &RemoteBandwidthReservation,
    completed_at: DateTime<Utc>,
) -> io::Result<()> {
    // Another concurrent attempt can legitimately settle a later wall-time
    // observation first. Compare only with this exact reservation's start;
    // the global marker is max-preserving when this completion is committed.
    if completed_at < reservation.started_at {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote bandwidth completion clock moved backwards; reservation retained",
        ));
    }
    Ok(())
}

fn find_reservation(
    ledger: &StoredRemoteBandwidthLedger,
    reservation: &RemoteBandwidthReservation,
) -> io::Result<usize> {
    ledger
        .entries
        .iter()
        .position(|entry| {
            entry.id == reservation.id
                && entry.host_id == reservation.host_id
                && entry.node_id == reservation.node_id
                && entry.recorded_at == reservation.started_at
                && entry.bytes == reservation.reserved_bytes
                && entry.kind == LedgerEntryKind::Reserved
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "remote bandwidth reservation is missing or no longer current",
            )
        })
}

fn window_duration() -> TimeDelta {
    TimeDelta::hours(REMOTE_BANDWIDTH_WINDOW_HOURS)
}

fn utc_minute_key(value: DateTime<Utc>) -> i64 {
    value.timestamp().div_euclid(60)
}

fn allocate_entry_id(entries: &[LedgerEntry]) -> io::Result<String> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let mut random = [0_u8; ENTRY_ID_BYTES];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::other(format!("could not generate ledger ID: {error}")))?;
        if random.iter().all(|byte| *byte == 0) {
            continue;
        }
        let mut id = String::with_capacity(ENTRY_ID_HEX_BYTES);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in random {
            id.push(HEX[usize::from(byte >> 4)] as char);
            id.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        if !entries.iter().any(|entry| entry.id == id) {
            return Ok(id);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique remote bandwidth ledger ID",
    ))
}

fn validate_entry_id(id: &str) -> io::Result<()> {
    if id.len() == ENTRY_ID_HEX_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !id.bytes().all(|byte| byte == b'0')
    {
        Ok(())
    } else {
        Err(invalid_data(
            "remote bandwidth ledger contains an invalid entry ID",
        ))
    }
}

fn validate_host_id(id: &str) -> io::Result<()> {
    if host_id_has_valid_shape(id) {
        Ok(())
    } else {
        Err(invalid_input(format!(
            "remote host ID must contain 1 to {MAX_HOST_ID_BYTES} bytes and use only the configured host-ID alphabet"
        )))
    }
}

fn validate_stored_host_id(id: &str) -> io::Result<()> {
    if host_id_has_valid_shape(id) {
        Ok(())
    } else {
        Err(invalid_data(
            "remote bandwidth ledger contains an invalid host ID",
        ))
    }
}

fn host_id_has_valid_shape(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_HOST_ID_BYTES {
        return false;
    }
    let bytes = id.as_bytes();
    bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_')
}

fn source_storage_key(host_id: &str, node_id: Option<&NodeId>) -> String {
    match node_id {
        Some(node_id) => format!("node:{}", node_id.as_str()),
        None => format!("host:{host_id}"),
    }
}

fn serialize_ledger(ledger: &StoredRemoteBandwidthLedger) -> io::Result<Vec<u8>> {
    ledger.validate()?;
    let mut contents = serde_json::to_vec_pretty(ledger)
        .map_err(|error| invalid_data(format!("invalid remote bandwidth ledger: {error}")))?;
    contents.push(b'\n');
    if contents.len() as u64 > MAX_BUDGET_FILE_BYTES {
        return Err(invalid_data("remote bandwidth ledger file is too large"));
    }
    Ok(contents)
}

fn deserialize_ledger(contents: &[u8]) -> io::Result<StoredRemoteBandwidthLedger> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct VersionProbe {
        schema_version: Option<u32>,
    }
    let version = serde_json::from_slice::<VersionProbe>(contents)
        .map_err(|error| invalid_data(format!("invalid remote bandwidth ledger: {error}")))?
        .schema_version
        .ok_or_else(|| invalid_data("remote bandwidth ledger is missing schemaVersion"))?;
    if version != REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION {
        let relation = if version > REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION {
            "future"
        } else {
            "unsupported"
        };
        return Err(invalid_data(format!(
            "{relation} remote bandwidth schema version {version}; expected {REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION}"
        )));
    }
    let ledger = serde_json::from_slice::<StoredRemoteBandwidthLedger>(contents)
        .map_err(|error| invalid_data(format!("invalid remote bandwidth ledger: {error}")))?;
    ledger.validate()?;
    Ok(ledger)
}

fn read_optional_ledger(path: &Path) -> io::Result<StoredRemoteBandwidthLedger> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoredRemoteBandwidthLedger::default());
        }
        Err(error) => return Err(error),
    }
    let contents =
        read_private_bounded(path, MAX_BUDGET_FILE_BYTES, "remote bandwidth ledger file")?;
    deserialize_ledger(&contents)
}

fn write_ledger_atomically(path: &Path, ledger: &StoredRemoteBandwidthLedger) -> io::Result<()> {
    let contents = serialize_ledger(ledger)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("remote bandwidth ledger path has no parent"))?;
    validate_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "remote bandwidth ledger file")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let (temporary_path, mut temporary) = create_temporary_file(parent, BUDGET_FILE)?;
    let result = (|| {
        temporary.write_all(&contents)?;
        temporary.sync_all()?;
        drop(temporary);
        replace_file(&temporary_path, path)?;
        validate_published_private_file(path, "remote bandwidth ledger file")?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn read_private_bounded(path: &Path, maximum: u64, subject: &str) -> io::Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    let metadata = file.metadata()?;
    validate_private_file_metadata(&metadata, subject)?;
    ensure_private_file(path, &file, &metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, subject)?;
    if metadata.len() > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    Ok(contents)
}

fn open_locked_lock_file(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(BUDGET_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "remote bandwidth ledger lock")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    add_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(stable_lock_share_mode());
    }
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "remote bandwidth ledger lock"))?;
    validate_opened_private_file(&path, &file, "remote bandwidth ledger lock")?;
    fs2::FileExt::lock_exclusive(&file)?;
    validate_private_directory(directory)?;
    validate_opened_private_file(&path, &file, "remote bandwidth ledger lock")?;
    Ok(file)
}

fn open_existing_shared_lock_file(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(BUDGET_LOCK_FILE);
    let metadata = fs::symlink_metadata(&path)?;
    validate_private_file_metadata(&metadata, "remote bandwidth ledger lock")?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(stable_lock_share_mode());
    }
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "remote bandwidth ledger lock"))?;
    validate_opened_private_file(&path, &file, "remote bandwidth ledger lock")?;
    fs2::FileExt::lock_shared(&file)?;
    validate_private_directory(directory)?;
    validate_opened_private_file(&path, &file, "remote bandwidth ledger lock")?;
    Ok(file)
}

fn validate_opened_private_file(path: &Path, file: &File, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&path_metadata, subject)?;
    let opened_metadata = file.metadata()?;
    validate_private_file_metadata(&opened_metadata, subject)?;
    ensure_private_file(path, file, &opened_metadata, subject)?;
    ensure_opened_file_matches_path(path, file, &path_metadata, &opened_metadata, subject)
}

fn create_temporary_file(parent: &Path, file_name: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        add_nofollow_flags(&mut options);
        match options.open(&path) {
            Ok(file) => {
                validate_opened_private_file(
                    &path,
                    &file,
                    "remote bandwidth ledger temporary file",
                )?;
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a remote bandwidth temporary file",
    ))
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    validate_opened_private_file(path, &file, subject)
}

fn create_private_directory_beneath(root: &Path, path: &Path) -> io::Result<()> {
    if !root.is_absolute() {
        return Err(invalid_input(
            "remote bandwidth state root must be absolute",
        ));
    }
    match validate_state_root(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_state_root(root)?,
        Err(error) => return Err(error),
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid_input("remote bandwidth path is outside its state root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(invalid_input(
                "remote bandwidth path contains a non-normal component",
            ));
        };
        current.push(name);
        match validate_private_directory(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_child_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    validate_private_directory(path)
}

fn create_state_root(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    validate_state_root(path)
}

fn create_private_child_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    #[cfg(not(unix))]
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_private_directory(path)
}

fn validate_state_root(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_data(
            "remote bandwidth state root must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote bandwidth state root must be owned by the current user",
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("remote bandwidth state root must have mode 0700 (found {mode:04o})"),
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_directory(path, "remote bandwidth state root")?;
    Ok(())
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_data(
            "remote bandwidth directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data("remote bandwidth path must be a directory"));
    }
    ensure_private_path(&metadata, "remote bandwidth directory")?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "remote bandwidth directory")?;
    Ok(())
}

fn validate_private_file_metadata(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "{subject} must not be a symbolic link or reparse point"
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_data(format!("{subject} must be a regular file")));
    }
    ensure_private_path(metadata, subject)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_path(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current user"),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must not be accessible by group or other users"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    Ok(())
}

fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    #[cfg(windows)]
    validate_windows_private_file(path, file, subject)?;
    #[cfg(not(windows))]
    let _ = (path, file, subject);
    Ok(())
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
    {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn ensure_opened_file_matches_path(
    path: &Path,
    opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    if windows_file_identity(&current)? == windows_file_identity(opened_file)? {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the live handle and output pointer are valid for this call.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported that it initialized the output structure.
    let information = unsafe { information.assume_init() };
    Ok((
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

fn add_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(any(windows, test))]
fn stable_lock_share_mode() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    #[cfg(not(windows))]
    {
        0x0000_0001 | 0x0000_0002
    }
}

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_data(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::TimeZone;
    use tempfile::tempdir;

    use crate::remote_ingest_state::RemoteDeltaNextRequestPosition;
    use crate::remote_sync::RemoteSyncCompletion;

    const NODE_A: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NODE_B: &str = "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn at(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, second)
            .single()
            .unwrap()
    }

    fn test_store(directory: &tempfile::TempDir) -> RemoteBandwidthBudgetStore {
        RemoteBandwidthBudgetStore::new(directory.path().join("state"))
    }

    fn node(value: &str) -> NodeId {
        NodeId::from_str(value).unwrap()
    }

    fn granted(admission: RemoteBandwidthAdmission) -> RemoteBandwidthReservation {
        match admission {
            RemoteBandwidthAdmission::Granted(reservation) => reservation,
            RemoteBandwidthAdmission::Paused(pause) => panic!("unexpected pause: {pause:?}"),
        }
    }

    fn paused(admission: RemoteBandwidthAdmission) -> RemoteBandwidthBudgetPause {
        match admission {
            RemoteBandwidthAdmission::Paused(pause) => pause,
            RemoteBandwidthAdmission::Granted(reservation) => {
                panic!("unexpected reservation: {reservation:?}")
            }
        }
    }

    fn charge(
        store: &RemoteBandwidthBudgetStore,
        host: &str,
        node: Option<&NodeId>,
        at: DateTime<Utc>,
        bytes: u64,
    ) {
        let reservation = granted(
            store
                .begin_attempt(
                    host,
                    node,
                    at,
                    RemoteBandwidthTransferKind::ManualOverride,
                    1,
                )
                .unwrap(),
        );
        store
            .complete_attempt(&reservation, at, usize::try_from(bytes).unwrap())
            .unwrap();
    }

    #[test]
    fn reservations_are_durable_and_settled_to_explicit_total_bytes() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node = node(NODE_A);
        let started = at(1, 12, 0, 0);
        let reservation = granted(
            store
                .begin_attempt(
                    "alpha",
                    Some(&node),
                    started,
                    RemoteBandwidthTransferKind::Manual,
                    16 * 1024 * 1024,
                )
                .unwrap(),
        );
        assert_eq!(reservation.reserved_bytes(), 16 * 1024 * 1024);
        assert_eq!(
            store
                .usage("other-alias", Some(&node), started)
                .unwrap()
                .unwrap()
                .reserved_bytes(),
            16 * 1024 * 1024
        );

        let settled = store
            .complete_attempt(&reservation, at(1, 12, 0, 5), 2 * 1024 * 1024)
            .unwrap();
        assert_eq!(settled.committed_bytes(), 2 * 1024 * 1024);
        assert_eq!(settled.reserved_bytes(), 0);

        let reopened = test_store(&directory);
        assert_eq!(
            reopened
                .usage("renamed", Some(&node), at(1, 12, 1, 0))
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn sync_reservation_precharges_transport_overhead_without_reducing_payload_limit() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let now = at(1, 12, 30, 0);
        let payload_limit = 16 * 1024 * 1024;
        let exchanges = 4;
        let overhead = exchanges * REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES;
        let reservation = granted(
            store
                .begin_sync_attempt(
                    "alpha",
                    None,
                    now,
                    RemoteBandwidthTransferKind::Manual,
                    payload_limit,
                    exchanges,
                )
                .unwrap(),
        );

        assert_eq!(
            reservation.reserved_bytes(),
            payload_limit as u64 + overhead as u64
        );
        assert_eq!(reservation.granted_response_bytes().unwrap(), payload_limit);
        let usage = store
            .complete_report(
                &reservation,
                now,
                &RemoteSyncReport {
                    pages_committed: exchanges,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: payload_limit,
                    completion: RemoteSyncCompletion::Complete,
                },
            )
            .unwrap();
        assert_eq!(usage.rolling_bytes(), reservation.reserved_bytes());
        assert!(usage.rolling_bytes() < REMOTE_BANDWIDTH_HARD_LIMIT_BYTES);
    }

    #[test]
    fn sync_reservation_pauses_before_ssh_when_capacity_cannot_fit_one_page_and_overhead() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let now = at(1, 12, 45, 0);
        let exchanges = 4;
        let minimum = (exchanges * REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES
            + MIN_REMOTE_SYNC_RESPONSE_BYTES) as u64;
        charge(
            &store,
            "alpha",
            None,
            now,
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES - minimum + 1,
        );

        let pause = paused(
            store
                .begin_sync_attempt(
                    "alpha",
                    None,
                    at(1, 12, 45, 1),
                    RemoteBandwidthTransferKind::Manual,
                    16 * 1024 * 1024,
                    exchanges,
                )
                .unwrap(),
        );
        assert_eq!(pause.level(), RemoteBandwidthBudgetLevel::Hard);
        assert_eq!(pause.usage().reserved_bytes(), 0);
        assert_eq!(
            pause.usage().rolling_bytes(),
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES - minimum + 1
        );
    }

    #[test]
    fn fixed_probe_crosses_hard_limit_and_charges_actual_response_plus_one_exchange() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node = node(NODE_A);
        let now = at(1, 13, 0, 0);
        charge(
            &store,
            "alpha",
            Some(&node),
            now,
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
        );

        assert!(
            store
                .begin_attempt(
                    "alpha",
                    Some(&node),
                    now,
                    RemoteBandwidthTransferKind::AutomaticProbe,
                    REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES,
                )
                .is_err(),
            "only the dedicated fixed-size probe API may bypass the hard cap"
        );
        let reservation = granted(
            store
                .begin_automatic_probe_attempt("alpha", Some(&node), now)
                .unwrap(),
        );
        assert_eq!(
            reservation.kind(),
            RemoteBandwidthTransferKind::AutomaticProbe
        );
        assert_eq!(
            reservation.granted_response_bytes().unwrap(),
            REMOTE_BANDWIDTH_AUTOMATIC_PROBE_MAX_RESPONSE_BYTES
        );
        let response_bytes = 1_337;
        let usage = store
            .complete_automatic_probe_attempt(&reservation, now, response_bytes)
            .unwrap();
        assert_eq!(
            usage.rolling_bytes(),
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES
                + response_bytes as u64
                + REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES as u64
        );
    }

    #[test]
    fn report_network_estimate_charges_one_four_and_zero_page_exchanges() {
        assert_eq!(REMOTE_BANDWIDTH_ESTIMATED_BYTES_PER_SSH_HOP, 100 * 1024);
        assert_eq!(REMOTE_BANDWIDTH_UNKNOWN_EFFECTIVE_HOPS, 3);
        assert_eq!(
            REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES,
            300 * 1024
        );
        let one_page = RemoteSyncReport {
            pages_committed: 1,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: 1_000,
            completion: RemoteSyncCompletion::Complete,
        };
        assert_eq!(
            estimated_remote_sync_network_bytes(&one_page).unwrap(),
            1_000 + 300 * 1024
        );
        let four_pages = RemoteSyncReport {
            pages_committed: 4,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: 2_000,
            completion: RemoteSyncCompletion::Complete,
        };
        assert_eq!(
            estimated_remote_sync_network_bytes(&four_pages).unwrap(),
            2_000 + 4 * 300 * 1024
        );
        let restarted_without_a_committed_page = RemoteSyncReport {
            pages_committed: 0,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: 0,
            completion: RemoteSyncCompletion::BootstrapRestarted(RemoteDeltaNextRequestPosition {
                delta_cursor: None,
                exact_range: None,
                known_live_revision: None,
            }),
        };
        assert_eq!(
            estimated_remote_sync_network_bytes(&restarted_without_a_committed_page).unwrap(),
            300 * 1024
        );
    }

    #[test]
    fn report_network_estimate_and_settlement_fail_closed_at_numeric_and_entry_bounds() {
        let response_overflow = RemoteSyncReport {
            pages_committed: 1,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: usize::MAX,
            completion: RemoteSyncCompletion::Complete,
        };
        assert_eq!(
            estimated_remote_sync_network_bytes(&response_overflow)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let overhead_overflow = RemoteSyncReport {
            pages_committed: usize::MAX,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: 0,
            completion: RemoteSyncCompletion::Complete,
        };
        assert_eq!(
            estimated_remote_sync_network_bytes(&overhead_overflow)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let maximum = usize::try_from(MAX_BYTES_PER_ENTRY).unwrap();
        let maximum_payload = maximum - REMOTE_BANDWIDTH_ESTIMATED_SSH_EXCHANGE_OVERHEAD_BYTES;
        let exact_directory = tempdir().unwrap();
        let exact_store = test_store(&exact_directory);
        let exact_at = at(1, 13, 0, 0);
        let exact_reservation = granted(
            exact_store
                .begin_attempt(
                    "exact",
                    None,
                    exact_at,
                    RemoteBandwidthTransferKind::ManualOverride,
                    1,
                )
                .unwrap(),
        );
        let exact_usage = exact_store
            .complete_report(
                &exact_reservation,
                exact_at,
                &RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: maximum_payload,
                    completion: RemoteSyncCompletion::Complete,
                },
            )
            .unwrap();
        assert_eq!(exact_usage.rolling_bytes(), MAX_BYTES_PER_ENTRY);

        let excessive_directory = tempdir().unwrap();
        let excessive_store = test_store(&excessive_directory);
        let excessive_reservation = granted(
            excessive_store
                .begin_attempt(
                    "excessive",
                    None,
                    exact_at,
                    RemoteBandwidthTransferKind::ManualOverride,
                    1,
                )
                .unwrap(),
        );
        let error = excessive_store
            .complete_report(
                &excessive_reservation,
                exact_at,
                &RemoteSyncReport {
                    pages_committed: 1,
                    changes_committed: 0,
                    live_state_changed: false,
                    response_bytes: maximum_payload + 1,
                    completion: RemoteSyncCompletion::Complete,
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            excessive_store
                .usage("excessive", None, exact_at)
                .unwrap()
                .unwrap()
                .reserved_bytes(),
            1
        );
    }

    #[test]
    fn complete_report_settles_payload_plus_estimated_exchange_overhead() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let now = at(1, 14, 0, 0);
        for (host_id, pages, response_bytes, expected) in [
            ("one", 1, 1_000, 1_000 + 300 * 1024),
            ("four", 4, 2_000, 2_000 + 4 * 300 * 1024),
            ("zero", 0, 0, 300 * 1024),
        ] {
            let reservation = granted(
                store
                    .begin_attempt(
                        host_id,
                        None,
                        now,
                        RemoteBandwidthTransferKind::ManualOverride,
                        1,
                    )
                    .unwrap(),
            );
            let usage = store
                .complete_report(
                    &reservation,
                    now,
                    &RemoteSyncReport {
                        pages_committed: pages,
                        changes_committed: 0,
                        live_state_changed: false,
                        response_bytes,
                        completion: if pages == 0 {
                            RemoteSyncCompletion::BootstrapRestarted(
                                RemoteDeltaNextRequestPosition {
                                    delta_cursor: None,
                                    exact_range: None,
                                    known_live_revision: None,
                                },
                            )
                        } else {
                            RemoteSyncCompletion::Complete
                        },
                    },
                )
                .unwrap();
            assert_eq!(usage.rolling_bytes(), expected as u64);
        }
    }

    #[test]
    fn soft_pauses_bulk_but_not_incremental_and_hard_pauses_manual() {
        let edge_directory = tempdir().unwrap();
        let edge_store = test_store(&edge_directory);
        let edge_node = node(NODE_A);
        let edge_now = at(2, 7, 0, 0);
        charge(
            &edge_store,
            "alpha",
            Some(&edge_node),
            edge_now,
            149 * 1024 * 1024,
        );
        let edge_reservation = granted(
            edge_store
                .begin_attempt(
                    "alpha",
                    Some(&edge_node),
                    at(2, 7, 1, 0),
                    RemoteBandwidthTransferKind::AutomaticBulk,
                    16 * 1024 * 1024,
                )
                .unwrap(),
        );
        assert_eq!(edge_reservation.reserved_bytes(), 1024 * 1024);
        assert_eq!(
            edge_store
                .usage("alpha", Some(&edge_node), at(2, 7, 1, 0))
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES
        );

        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node = node(NODE_A);
        let now = at(2, 8, 0, 0);
        charge(
            &store,
            "alpha",
            Some(&node),
            now,
            REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
        );

        let soft = paused(
            store
                .begin_attempt(
                    "alpha",
                    Some(&node),
                    at(2, 8, 1, 0),
                    RemoteBandwidthTransferKind::AutomaticBulk,
                    1024,
                )
                .unwrap(),
        );
        assert_eq!(soft.level(), RemoteBandwidthBudgetLevel::Soft);
        assert_eq!(soft.reason(), RemoteBandwidthPauseReason::LimitReached);
        assert_eq!(soft.resume_at(), Some(at(3, 8, 0, 0)));
        assert_eq!(
            store
                .check(
                    "alpha",
                    Some(&node),
                    at(2, 8, 1, 0),
                    RemoteBandwidthTransferKind::AutomaticBulk,
                )
                .unwrap()
                .unwrap_err()
                .level(),
            RemoteBandwidthBudgetLevel::Soft
        );
        assert!(
            store
                .check(
                    "alpha",
                    Some(&node),
                    at(2, 8, 1, 0),
                    RemoteBandwidthTransferKind::Manual,
                )
                .unwrap()
                .is_ok()
        );

        let incremental = granted(
            store
                .begin_attempt(
                    "alpha",
                    Some(&node),
                    at(2, 8, 2, 0),
                    RemoteBandwidthTransferKind::AutomaticIncremental,
                    1024,
                )
                .unwrap(),
        );
        store
            .complete_attempt(&incremental, at(2, 8, 2, 1), 1024)
            .unwrap();

        let hard_directory = tempdir().unwrap();
        let hard_store = test_store(&hard_directory);
        charge(
            &hard_store,
            "alpha",
            Some(&node),
            now,
            REMOTE_BANDWIDTH_HARD_LIMIT_BYTES,
        );
        for kind in [
            RemoteBandwidthTransferKind::AutomaticBulk,
            RemoteBandwidthTransferKind::AutomaticIncremental,
            RemoteBandwidthTransferKind::Manual,
        ] {
            let hard = paused(
                hard_store
                    .begin_attempt("alpha", Some(&node), at(2, 8, 1, 0), kind, 1024)
                    .unwrap(),
            );
            assert_eq!(hard.level(), RemoteBandwidthBudgetLevel::Hard);
            assert!(hard.budget_paused());
        }
        assert!(matches!(
            hard_store
                .begin_attempt(
                    "alpha",
                    Some(&node),
                    at(2, 8, 1, 0),
                    RemoteBandwidthTransferKind::ManualOverride,
                    1024,
                )
                .unwrap(),
            RemoteBandwidthAdmission::Granted(_)
        ));
    }

    #[test]
    fn rolling_window_expires_at_24_hours_and_reports_resume_time() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let first = at(4, 10, 30, 0);
        charge(
            &store,
            "alpha",
            None,
            first,
            REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
        );
        let pause = paused(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    first + TimeDelta::hours(23),
                    RemoteBandwidthTransferKind::AutomaticBulk,
                    1024,
                )
                .unwrap(),
        );
        assert_eq!(pause.resume_at(), Some(first + TimeDelta::hours(24)));

        assert!(matches!(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    first + TimeDelta::hours(24),
                    RemoteBandwidthTransferKind::AutomaticBulk,
                    1024,
                )
                .unwrap(),
            RemoteBandwidthAdmission::Granted(_)
        ));
    }

    #[test]
    fn clock_rollback_is_a_structured_hard_pause_and_retains_reservation() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let future = at(6, 10, 0, 0);
        let reservation = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    future,
                    RemoteBandwidthTransferKind::Manual,
                    4096,
                )
                .unwrap(),
        );
        let pause = store
            .usage("alpha", None, future - TimeDelta::minutes(1))
            .unwrap()
            .unwrap_err();
        assert_eq!(pause.level(), RemoteBandwidthBudgetLevel::Hard);
        assert_eq!(pause.reason(), RemoteBandwidthPauseReason::ClockAnomaly);
        assert_eq!(pause.resume_at(), Some(future));
        assert_eq!(pause.usage().reserved_bytes(), 4096);

        let error = store
            .cancel_attempt(&reservation, future - TimeDelta::seconds(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            store
                .usage("alpha", None, future)
                .unwrap()
                .unwrap()
                .reserved_bytes(),
            4096
        );
    }

    #[test]
    fn concurrent_reservations_never_exceed_hard_cap() {
        let directory = tempdir().unwrap();
        let store = Arc::new(test_store(&directory));
        let barrier = Arc::new(Barrier::new(5));
        let now = at(7, 9, 0, 0);
        let mut workers = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                store
                    .begin_attempt(
                        "alpha",
                        None,
                        now,
                        RemoteBandwidthTransferKind::Manual,
                        100 * 1024 * 1024,
                    )
                    .unwrap()
            }));
        }
        barrier.wait();
        let admissions = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(admissions.iter().any(|admission| matches!(
            admission,
            RemoteBandwidthAdmission::Paused(RemoteBandwidthBudgetPause {
                level: RemoteBandwidthBudgetLevel::Hard,
                ..
            })
        )));
        let usage = store.usage("alpha", None, now).unwrap().unwrap();
        assert_eq!(usage.reserved_bytes(), REMOTE_BANDWIDTH_HARD_LIMIT_BYTES);
        assert!(usage.rolling_bytes() <= REMOTE_BANDWIDTH_HARD_LIMIT_BYTES);
    }

    #[test]
    fn node_identity_scopes_budget_across_host_aliases() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node_a = node(NODE_A);
        let node_b = node(NODE_B);
        let now = at(8, 12, 0, 0);
        charge(&store, "alpha", Some(&node_a), now, 5000);
        assert_eq!(
            store
                .usage("renamed-alpha", Some(&node_a), now)
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            5000
        );
        assert_eq!(
            store
                .usage("alpha", Some(&node_b), now)
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            0
        );
        assert_eq!(
            store
                .usage("alpha", None, now)
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            0
        );
    }

    #[test]
    fn cancel_removes_only_the_exact_reservation() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let now = at(9, 12, 0, 0);
        let first = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    now,
                    RemoteBandwidthTransferKind::Manual,
                    4096,
                )
                .unwrap(),
        );
        let second = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    now,
                    RemoteBandwidthTransferKind::Manual,
                    8192,
                )
                .unwrap(),
        );
        let usage = store.cancel_attempt(&first, now).unwrap();
        assert_eq!(usage.reserved_bytes(), 8192);
        assert!(store.complete_attempt(&first, now, 1).is_err());
        assert_eq!(
            store
                .complete_attempt(&second, now, 12)
                .unwrap()
                .rolling_bytes(),
            12
        );
    }

    #[test]
    fn concurrent_attempts_may_complete_out_of_wall_time_order() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let first_at = at(9, 13, 0, 0);
        let first = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    first_at,
                    RemoteBandwidthTransferKind::Manual,
                    4096,
                )
                .unwrap(),
        );
        let second = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    first_at + TimeDelta::seconds(2),
                    RemoteBandwidthTransferKind::Manual,
                    4096,
                )
                .unwrap(),
        );
        store
            .complete_attempt(&second, first_at + TimeDelta::seconds(4), 200)
            .unwrap();
        let usage = store
            .complete_attempt(&first, first_at + TimeDelta::seconds(1), 100)
            .unwrap();
        assert_eq!(usage.rolling_bytes(), 300);
        assert_eq!(
            store
                .usage("alpha", None, first_at + TimeDelta::seconds(4))
                .unwrap()
                .unwrap()
                .rolling_bytes(),
            300
        );
    }

    #[test]
    fn committed_events_in_one_minute_are_compacted_conservatively() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        charge(&store, "alpha", None, at(10, 12, 0, 1), 100);
        charge(&store, "alpha", None, at(10, 12, 0, 59), 200);
        let ledger = read_optional_ledger(&store.budget_path()).unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].bytes, 300);
        assert_eq!(ledger.entries[0].recorded_at, at(10, 12, 0, 59));
    }

    #[test]
    fn interleaved_sources_compact_by_stable_identity_and_keep_reservations_distinct() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node_a = node(NODE_A);
        let node_b = node(NODE_B);
        charge(&store, "alpha-old", Some(&node_a), at(10, 13, 0, 1), 100);
        charge(&store, "beta", Some(&node_b), at(10, 13, 0, 2), 200);
        charge(
            &store,
            "alpha-renamed",
            Some(&node_a),
            at(10, 13, 0, 3),
            300,
        );
        charge(&store, "beta", Some(&node_b), at(10, 13, 0, 4), 400);

        let first_reservation = granted(
            store
                .begin_attempt(
                    "alpha-renamed",
                    Some(&node_a),
                    at(10, 13, 0, 5),
                    RemoteBandwidthTransferKind::ManualOverride,
                    10,
                )
                .unwrap(),
        );
        let second_reservation = granted(
            store
                .begin_attempt(
                    "alpha-renamed",
                    Some(&node_a),
                    at(10, 13, 0, 6),
                    RemoteBandwidthTransferKind::ManualOverride,
                    20,
                )
                .unwrap(),
        );
        // Completing an interleaved source runs compaction while both
        // reservations are live; their independently settleable IDs must stay.
        charge(&store, "beta", Some(&node_b), at(10, 13, 0, 7), 500);

        let ledger = read_optional_ledger(&store.budget_path()).unwrap();
        ledger.validate().unwrap();
        let committed_a = ledger
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == LedgerEntryKind::Committed && entry.node_id.as_ref() == Some(&node_a)
            })
            .collect::<Vec<_>>();
        let committed_b = ledger
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == LedgerEntryKind::Committed && entry.node_id.as_ref() == Some(&node_b)
            })
            .collect::<Vec<_>>();
        let reserved_a = ledger
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == LedgerEntryKind::Reserved && entry.node_id.as_ref() == Some(&node_a)
            })
            .collect::<Vec<_>>();
        assert_eq!(committed_a.len(), 1);
        assert_eq!(committed_a[0].bytes, 400);
        assert_eq!(committed_a[0].recorded_at, at(10, 13, 0, 3));
        assert_eq!(committed_b.len(), 1);
        assert_eq!(committed_b[0].bytes, 1_100);
        assert_eq!(committed_b[0].recorded_at, at(10, 13, 0, 7));
        assert_eq!(reserved_a.len(), 2);
        assert!(
            reserved_a
                .iter()
                .any(|entry| entry.id == first_reservation.id)
        );
        assert!(
            reserved_a
                .iter()
                .any(|entry| entry.id == second_reservation.id)
        );
    }

    #[test]
    fn source_minute_compaction_keeps_multiple_entries_at_the_entry_byte_bound() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node_a = node(NODE_A);
        let node_b = node(NODE_B);
        charge(
            &store,
            "alpha",
            Some(&node_a),
            at(10, 14, 0, 1),
            MAX_BYTES_PER_ENTRY - 10,
        );
        charge(&store, "beta", Some(&node_b), at(10, 14, 0, 2), 1);
        charge(&store, "alpha", Some(&node_a), at(10, 14, 0, 3), 20);
        charge(&store, "alpha", Some(&node_a), at(10, 14, 0, 4), 5);
        let ledger = read_optional_ledger(&store.budget_path()).unwrap();
        ledger.validate().unwrap();
        let alpha = ledger
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == LedgerEntryKind::Committed && entry.node_id.as_ref() == Some(&node_a)
            })
            .collect::<Vec<_>>();
        assert_eq!(alpha.len(), 2);
        assert_eq!(alpha[0].bytes, MAX_BYTES_PER_ENTRY - 10);
        assert_eq!(alpha[1].bytes, 25);
        assert_eq!(alpha[1].recorded_at, at(10, 14, 0, 4));
        assert_eq!(
            alpha.iter().map(|entry| entry.bytes).sum::<u64>(),
            MAX_BYTES_PER_ENTRY + 15
        );
    }

    #[test]
    fn thirty_second_interleaved_samples_remain_below_capacity_past_twenty_four_hours() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node_a = node(NODE_A);
        let node_b = node(NODE_B);
        let start = at(12, 0, 0, 0);
        let samples_per_source = 25 * 60 * 2;
        let mut entries = Vec::with_capacity(samples_per_source * 2);
        for sample in 0..samples_per_source {
            let recorded_at = start + TimeDelta::seconds((sample * 30) as i64);
            for (source_index, (host_id, node_id)) in [("alpha", &node_a), ("beta", &node_b)]
                .into_iter()
                .enumerate()
            {
                let id = sample * 2 + source_index + 1;
                entries.push(LedgerEntry {
                    id: format!("{id:032x}"),
                    host_id: host_id.to_owned(),
                    node_id: Some(node_id.clone()),
                    recorded_at,
                    bytes: 1,
                    kind: LedgerEntryKind::Committed,
                });
            }
        }
        let now = start + TimeDelta::hours(25);
        let mut ledger = StoredRemoteBandwidthLedger {
            schema_version: REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION,
            last_observed_at: Some(now),
            entries,
        };
        ledger.merge_committed_minute_buckets().unwrap();
        ledger.validate().unwrap();
        assert_eq!(ledger.source_entry_count("alpha", Some(&node_a)), 25 * 60);
        assert_eq!(ledger.source_entry_count("beta", Some(&node_b)), 25 * 60);
        store
            .mutate(|stored| {
                *stored = ledger;
                Ok(())
            })
            .unwrap();

        let admission = store
            .begin_attempt(
                "alpha-renamed",
                Some(&node_a),
                now,
                RemoteBandwidthTransferKind::AutomaticIncremental,
                1,
            )
            .unwrap();
        assert!(matches!(admission, RemoteBandwidthAdmission::Granted(_)));
        let persisted = read_optional_ledger(&store.budget_path()).unwrap();
        assert!(persisted.source_entry_count("alpha", Some(&node_a)) < MAX_ENTRIES_PER_SOURCE);
        assert!(persisted.source_entry_count("beta", Some(&node_b)) < MAX_ENTRIES_PER_SOURCE);
    }

    #[test]
    fn admission_compacts_an_existing_interleaved_ledger_at_the_source_cap() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let node_a = node(NODE_A);
        let node_b = node(NODE_B);
        let start = at(14, 0, 0, 0);
        let mut entries = Vec::with_capacity(MAX_ENTRIES_PER_SOURCE * 2);
        for sample in 0..MAX_ENTRIES_PER_SOURCE {
            let recorded_at = start + TimeDelta::seconds((sample * 30) as i64);
            for (source_index, (host_id, node_id)) in [("alpha", &node_a), ("beta", &node_b)]
                .into_iter()
                .enumerate()
            {
                let id = sample * 2 + source_index + 1;
                entries.push(LedgerEntry {
                    id: format!("{id:032x}"),
                    host_id: host_id.to_owned(),
                    node_id: Some(node_id.clone()),
                    recorded_at,
                    bytes: 1,
                    kind: LedgerEntryKind::Committed,
                });
            }
        }
        let now = start + TimeDelta::seconds((MAX_ENTRIES_PER_SOURCE * 30) as i64);
        let ledger = StoredRemoteBandwidthLedger {
            schema_version: REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION,
            last_observed_at: Some(now),
            entries,
        };
        ledger.validate().unwrap();
        assert_eq!(
            ledger.source_entry_count("alpha", Some(&node_a)),
            MAX_ENTRIES_PER_SOURCE
        );
        store
            .mutate(|stored| {
                *stored = ledger;
                Ok(())
            })
            .unwrap();

        let admission = store
            .begin_attempt(
                "alpha-new-alias",
                Some(&node_a),
                now,
                RemoteBandwidthTransferKind::AutomaticIncremental,
                1,
            )
            .unwrap();
        assert!(matches!(admission, RemoteBandwidthAdmission::Granted(_)));
        let compacted = read_optional_ledger(&store.budget_path()).unwrap();
        assert!(compacted.source_entry_count("alpha", Some(&node_a)) < MAX_ENTRIES_PER_SOURCE);
        assert!(compacted.source_entry_count("beta", Some(&node_b)) < MAX_ENTRIES_PER_SOURCE);
    }

    #[cfg(unix)]
    #[test]
    fn state_is_private_and_symlink_ledger_is_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let now = at(11, 12, 0, 0);
        let reservation = granted(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    now,
                    RemoteBandwidthTransferKind::Manual,
                    1024,
                )
                .unwrap(),
        );
        store.complete_attempt(&reservation, now, 12).unwrap();
        let state_mode = fs::metadata(store.state_root())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = fs::metadata(store.budget_directory())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(store.budget_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            (state_mode, directory_mode, file_mode),
            (0o700, 0o700, 0o600)
        );

        fs::remove_file(store.budget_path()).unwrap();
        let target = directory.path().join("outside.json");
        fs::write(&target, b"{}\n").unwrap();
        symlink(&target, store.budget_path()).unwrap();
        assert!(store.usage("alpha", None, now).is_err());
    }

    #[test]
    fn future_schema_and_oversized_input_fail_closed() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        create_private_directory_beneath(store.state_root(), &store.budget_directory()).unwrap();
        let future = serde_json::json!({
            "schemaVersion": REMOTE_BANDWIDTH_BUDGET_SCHEMA_VERSION + 1,
            "entries": []
        });
        fs::write(
            store.budget_path(),
            serde_json::to_vec_pretty(&future).unwrap(),
        )
        .unwrap();
        assert!(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    at(12, 0, 0, 0),
                    RemoteBandwidthTransferKind::Manual,
                    1024,
                )
                .is_err()
        );

        let another = tempdir().unwrap();
        let store = test_store(&another);
        assert!(
            store
                .begin_attempt(
                    "alpha",
                    None,
                    at(12, 0, 0, 0),
                    RemoteBandwidthTransferKind::Manual,
                    0,
                )
                .is_err()
        );
    }

    #[test]
    fn lock_share_mode_keeps_delete_sharing_disabled_on_windows() {
        assert_eq!(stable_lock_share_mode(), 0x0000_0001 | 0x0000_0002);
    }
}
