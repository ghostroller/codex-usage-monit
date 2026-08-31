//! Single-worker scheduling policy for automatic remote synchronization.
//!
//! This module is intentionally only a state machine. It never discovers SSH
//! hosts, opens a connection, or writes history itself. The caller supplies a
//! validated [`RemotesConfig`] and an executor backed by the existing bounded
//! remote-sync orchestration. Eligible hosts come exclusively from
//! [`RemotesConfig::automatic_hosts`], so the fail-closed global and per-host
//! opt-ins remain the only automatic connection allowlist.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use crate::remote_bandwidth_budget::{RemoteBandwidthBudgetLevel, budget_pause_from_io_error};
use crate::remote_protocol::{MAX_REMOTE_FRAME_ENCODED_BYTES, SourceGeneration};
use crate::remote_sync::{
    RemoteSyncCompletion, RemoteSyncError, RemoteSyncHostSnapshot, RemoteSyncLimits,
    RemoteSyncReport,
};
use crate::remote_sync_health::REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS;
use crate::remotes_config::{RemoteHostConfig, RemotesConfig};
#[cfg(test)]
use crate::source_identity::NodeId;

/// Automatic runs deliberately use less bandwidth than an explicit manual
/// run: one page (at most one protocol frame) and a shorter exchange timeout.
/// The existing remote-sync layer applies its own stricter absolute caps too.
pub const AUTOMATIC_REMOTE_SYNC_MAX_PAGES: usize = 1;
pub const AUTOMATIC_REMOTE_SYNC_MAX_RESPONSE_BYTES: usize =
    MAX_REMOTE_FRAME_ENCODED_BYTES + REMOTE_FRAME_HEADER_BYTES;
pub const AUTOMATIC_REMOTE_SYNC_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep polling at the active cadence until this much continuous time has
/// elapsed without a committed bucket/digest or full live-state change.
pub const AUTOMATIC_REMOTE_SYNC_ACTIVITY_HYSTERESIS: Duration = Duration::from_secs(10 * 60);

const REMOTE_FRAME_HEADER_BYTES: usize = 20;
const MIN_DISPATCH_SPACING: Duration = Duration::from_secs(1);
const FAILURE_BACKOFF_SECONDS: [u64; 5] = [60, 2 * 60, 5 * 60, 10 * 60, 30 * 60];
const JITTER_PERCENT_MIN: u64 = 80;
const JITTER_PERCENT_SPAN: u64 = 41;

/// Monotonic time source injected into the scheduler.
///
/// Durations are measured from an arbitrary process-local origin. The
/// scheduler also clamps a regressing implementation to its last observation,
/// which makes test clocks and unusual platform clocks fail safe without a
/// zero-delay retry loop.
pub trait RemoteSyncSchedulerClock {
    fn now(&self) -> Duration;
}

/// Production monotonic clock. Wall-clock changes do not affect retry or
/// polling intervals; the sync executor remains responsible for the UTC range
/// passed to the remote protocol.
#[derive(Debug)]
pub struct MonotonicRemoteSyncClock {
    origin: Instant,
}

impl Default for MonotonicRemoteSyncClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl RemoteSyncSchedulerClock for MonotonicRemoteSyncClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Injectable execution boundary. A service adapter should call
/// `sync_remote_delta_bounded` with the supplied automatic snapshot and limits.
/// That existing orchestration rechecks the config revision and exact host pin
/// before every local commit, including when configuration changes in flight.
pub trait AutomaticRemoteSyncExecutor {
    /// Restores one process-start scheduling deadline from local durable state.
    ///
    /// The default keeps lightweight/test executors stateless. Production
    /// implementations may return a seed only for the exact full source pin in
    /// `host`; the scheduler also verifies that pin before accepting it.
    fn restore_host_schedule(
        &mut self,
        _host: &RemoteHostConfig,
    ) -> Result<Option<RemoteSyncScheduleSeed>, RemoteSyncError> {
        Ok(None)
    }

    /// Refreshes durable, host-bound containment pauses without opening SSH.
    /// `None` means this executor has no external health store, so the
    /// scheduler preserves its current in-process state.
    fn process_containment_paused_hosts(
        &mut self,
        _hosts: &[RemoteHostConfig],
    ) -> Result<Option<BTreeMap<String, bool>>, RemoteSyncError> {
        Ok(None)
    }

    /// Returns and clears a containment residual detected by a secondary SSH
    /// exchange while `sync_host` otherwise produced a valid aggregate result.
    fn take_process_containment_signal(&mut self, _host: &RemoteHostConfig) -> bool {
        false
    }

    fn sync_host(
        &mut self,
        selected: &RemoteSyncHostSnapshot,
        limits: RemoteSyncLimits,
    ) -> Result<RemoteSyncReport, RemoteSyncError>;
}

/// Process-local representation of a durable scheduling deadline.
///
/// `next_eligible_in` is computed from the persisted absolute UTC deadline by
/// the filesystem executor immediately before the scheduler consumes it. The
/// scheduler then converts it to its monotonic clock domain, so later wall
/// clock changes cannot create retry loops. `consecutive_failures` carries the
/// durable failure streak into the next backoff calculation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSyncScheduleSeed {
    source: SourceGeneration,
    next_eligible_in: Duration,
    consecutive_failures: u32,
    active_until_in: Option<Duration>,
    process_containment_paused: bool,
}

impl RemoteSyncScheduleSeed {
    pub fn new(
        source: SourceGeneration,
        next_eligible_in: Duration,
        consecutive_failures: u32,
    ) -> Self {
        Self {
            source,
            next_eligible_in,
            consecutive_failures,
            active_until_in: None,
            process_containment_paused: false,
        }
    }

    pub fn with_active_until_in(mut self, active_until_in: Option<Duration>) -> Self {
        self.active_until_in = active_until_in.filter(|delay| !delay.is_zero());
        self
    }

    pub fn with_process_containment_paused(mut self, paused: bool) -> Self {
        self.process_containment_paused = paused;
        self
    }

    pub fn source(&self) -> &SourceGeneration {
        &self.source
    }

    pub fn next_eligible_in(&self) -> Duration {
        self.next_eligible_in
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn active_until_in(&self) -> Option<Duration> {
        self.active_until_in
    }

    pub fn process_containment_paused(&self) -> bool {
        self.process_containment_paused
    }
}

/// Result of one scheduler tick. At most the `Attempted` variant can invoke
/// the executor, and it always represents exactly one host.
#[derive(Debug)]
pub enum RemoteSyncSchedulerTick {
    /// Global automatic synchronization is disabled. No host was inspected or
    /// executed.
    Disabled { next_wake_in: Duration },
    /// The global switch is on, but no explicitly paired, enabled host exists.
    NoEligibleHosts { next_wake_in: Duration },
    /// Eligible hosts exist, but their interval/backoff or the global dispatch
    /// spacing has not elapsed.
    Waiting { next_wake_in: Duration },
    /// One exact allowlist entry was passed to the injected executor.
    Attempted {
        host_id: String,
        /// Exact configuration revision that authorized this automatic
        /// attempt. Late health results are discarded after any configuration
        /// mutation, including global/host disablement or SSH alias edits.
        config_revision: u64,
        /// Sanitized source identity copied from the exact allowlist pin used
        /// for this attempt. Connection details are intentionally omitted.
        source: Option<SourceGeneration>,
        /// At least one SSH exchange in this attempt could not prove complete
        /// process-tree cleanup. The aggregate `result` may still be `Ok`
        /// because committed data remains valid.
        process_containment_uncertain: bool,
        result: Result<RemoteSyncReport, RemoteSyncError>,
        /// Delay from the completed attempt until this exact host becomes
        /// eligible again. Unlike `next_wake_in`, this is never shortened by
        /// another host or by global round-robin dispatch spacing.
        next_eligible_in: Duration,
        next_wake_in: Duration,
    },
}

impl RemoteSyncSchedulerTick {
    /// A strictly positive delay suitable for a service loop. Configuration
    /// file notification may wake the loop sooner.
    pub fn next_wake_in(&self) -> Duration {
        match self {
            Self::Disabled { next_wake_in }
            | Self::NoEligibleHosts { next_wake_in }
            | Self::Waiting { next_wake_in }
            | Self::Attempted { next_wake_in, .. } => *next_wake_in,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LastAttempt {
    Never,
    ActiveSuccess {
        finished_at: Duration,
    },
    IdleSuccess {
        finished_at: Duration,
    },
    BudgetPause {
        finished_at: Duration,
        hard: bool,
        consecutive_failures: u32,
    },
    ProcessContainmentPause,
    Failure {
        finished_at: Duration,
        consecutive_failures: u32,
    },
    Restored {
        due_at: Duration,
        consecutive_failures: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HostSchedule {
    /// Exact host metadata is retained only to notice SSH target, redaction,
    /// or source-pin changes. Such a change gets a fresh schedule; the actual
    /// response commit is still protected by the sync layer's config CAS.
    configured_host: RemoteHostConfig,
    last_attempt: LastAttempt,
    active_until: Option<Duration>,
}

impl HostSchedule {
    fn new(configured_host: RemoteHostConfig) -> Self {
        Self {
            configured_host,
            last_attempt: LastAttempt::Never,
            active_until: None,
        }
    }

    fn due_at(&self, config: &RemotesConfig) -> Option<Duration> {
        Some(match self.last_attempt {
            LastAttempt::Never => Duration::ZERO,
            LastAttempt::ActiveSuccess { finished_at } => {
                let interval_due = finished_at
                    .saturating_add(Duration::from_secs(config.active_interval_seconds()));
                self.active_until
                    .filter(|active_until| *active_until > finished_at)
                    .map_or(interval_due, |active_until| interval_due.min(active_until))
            }
            LastAttempt::IdleSuccess { finished_at } => {
                finished_at.saturating_add(Duration::from_secs(config.idle_interval_seconds()))
            }
            LastAttempt::BudgetPause {
                finished_at, hard, ..
            } => finished_at.saturating_add(if hard {
                Duration::from_secs(config.idle_interval_seconds()).min(Duration::from_secs(
                    REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS as u64,
                ))
            } else {
                Duration::from_secs(config.idle_interval_seconds())
            }),
            LastAttempt::Failure {
                finished_at,
                consecutive_failures,
            } => finished_at
                .saturating_add(retry_delay(self.configured_host.id(), consecutive_failures)),
            LastAttempt::ProcessContainmentPause => return None,
            LastAttempt::Restored { due_at, .. } => due_at,
        })
    }
}

/// One-process, one-worker automatic scheduler.
///
/// `tick` is synchronous and starts at most one host, so callers must drive a
/// single instance from one service thread. Round-robin selection applies
/// whenever multiple hosts are due. Host-specific schedules prevent one
/// failing remote from slowing independent remotes.
#[derive(Debug)]
pub struct RemoteSyncScheduler<C> {
    clock: C,
    hosts: BTreeMap<String, HostSchedule>,
    last_dispatched_host: Option<String>,
    last_dispatch_finished_at: Option<Duration>,
    last_observed_now: Duration,
    restore_durable_state: bool,
}

impl<C: RemoteSyncSchedulerClock> RemoteSyncScheduler<C> {
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            hosts: BTreeMap::new(),
            last_dispatched_host: None,
            last_dispatch_finished_at: None,
            last_observed_now: Duration::ZERO,
            restore_durable_state: true,
        }
    }

    /// Reconciles the current explicit allowlist and possibly runs one host.
    ///
    /// This method never calls `RemotesConfig::hosts` and never reads SSH
    /// configuration. The only selection source is `automatic_hosts`.
    pub fn tick(
        &mut self,
        config: &RemotesConfig,
        executor: &mut impl AutomaticRemoteSyncExecutor,
    ) -> Result<RemoteSyncSchedulerTick, RemoteSyncError> {
        let now = self.monotonic_now();

        if !config.auto_sync_enabled() {
            self.restore_durable_state = false;
            self.clear_ineligible_state();
            return Ok(RemoteSyncSchedulerTick::Disabled {
                next_wake_in: configured_idle_delay(config),
            });
        }

        let eligible = config.automatic_hosts().cloned().collect::<Vec<_>>();
        if eligible.is_empty() {
            self.restore_durable_state = false;
            self.clear_ineligible_state();
            return Ok(RemoteSyncSchedulerTick::NoEligibleHosts {
                next_wake_in: configured_idle_delay(config),
            });
        }

        let restore_durable_state = self.restore_durable_state;
        let durable_process_containment_pauses =
            executor.process_containment_paused_hosts(&eligible)?;
        self.reconcile_hosts(
            &eligible,
            executor,
            now,
            restore_durable_state,
            durable_process_containment_pauses.as_ref(),
        )?;
        self.restore_durable_state = false;
        let global_due = self
            .last_dispatch_finished_at
            .map(|finished| finished.saturating_add(MIN_DISPATCH_SPACING))
            .unwrap_or(Duration::ZERO);

        let Some(index) = self.next_due_host_index(&eligible, config, now, global_due) else {
            return Ok(RemoteSyncSchedulerTick::Waiting {
                next_wake_in: self.next_wake_delay(&eligible, config, now, global_due),
            });
        };

        let host = &eligible[index];
        // This is intentionally the automatic constructor, not the manual
        // path. It validates both opt-ins and the exact borrowed allowlist row.
        let selected = RemoteSyncHostSnapshot::capture_for_automatic(config, host)?;
        let host_id = host.id().to_owned();
        let source = host.expected_source().cloned();
        let limits = automatic_remote_sync_limits();
        let result = executor.sync_host(&selected, limits);
        let process_containment_uncertain = match result.as_ref() {
            Err(RemoteSyncError::ProcessContainment) => true,
            Err(RemoteSyncError::Transport(error)) => error.process_containment_uncertain(),
            _ => false,
        } || executor.take_process_containment_signal(host);
        let finished_at = self.monotonic_now();

        let previous_failures = self
            .hosts
            .get(&host_id)
            .and_then(|state| match state.last_attempt {
                LastAttempt::Failure {
                    consecutive_failures,
                    ..
                } => Some(consecutive_failures),
                LastAttempt::Restored {
                    consecutive_failures,
                    ..
                } if consecutive_failures > 0 => Some(consecutive_failures),
                LastAttempt::BudgetPause {
                    consecutive_failures,
                    ..
                } if consecutive_failures > 0 => Some(consecutive_failures),
                _ => None,
            })
            .unwrap_or(0);
        let schedule = self
            .hosts
            .get_mut(&host_id)
            .expect("the selected automatic host was reconciled");
        let last_attempt = if process_containment_uncertain {
            LastAttempt::ProcessContainmentPause
        } else {
            match result.as_ref() {
                Ok(report) => {
                    if report.has_activity() {
                        schedule.active_until = Some(
                            finished_at.saturating_add(AUTOMATIC_REMOTE_SYNC_ACTIVITY_HYSTERESIS),
                        );
                    } else if schedule
                        .active_until
                        .is_some_and(|active_until| active_until <= finished_at)
                    {
                        schedule.active_until = None;
                    }
                    if report_requires_immediate_active_polling(report)
                        || schedule
                            .active_until
                            .is_some_and(|active_until| active_until > finished_at)
                    {
                        LastAttempt::ActiveSuccess { finished_at }
                    } else {
                        LastAttempt::IdleSuccess { finished_at }
                    }
                }
                Err(RemoteSyncError::Local(error))
                    if budget_pause_from_io_error(error).is_some() =>
                {
                    // Budget pauses are a local policy decision, not a transport
                    // failure. Hard pauses wake at least every probe interval even
                    // when a user configured a much longer idle interval; the
                    // executor's durable deadline still gates the actual SSH
                    // probe. Soft bulk pauses retain the configured idle cadence.
                    LastAttempt::BudgetPause {
                        finished_at,
                        hard: budget_pause_from_io_error(error)
                            .is_some_and(|pause| pause.level() == RemoteBandwidthBudgetLevel::Hard),
                        consecutive_failures: previous_failures,
                    }
                }
                Err(_) => LastAttempt::Failure {
                    finished_at,
                    consecutive_failures: previous_failures.saturating_add(1),
                },
            }
        };
        schedule.last_attempt = last_attempt;
        self.last_dispatched_host = Some(host_id.clone());
        self.last_dispatch_finished_at = Some(finished_at);

        let next_eligible_in = self
            .hosts
            .get(&host_id)
            .expect("the attempted host remains scheduled")
            .due_at(config)
            .map_or(Duration::MAX, |due_at| due_at.saturating_sub(finished_at));
        debug_assert!(!next_eligible_in.is_zero());
        let next_global_due = finished_at.saturating_add(MIN_DISPATCH_SPACING);
        let next_wake_in = self.next_wake_delay(&eligible, config, finished_at, next_global_due);
        Ok(RemoteSyncSchedulerTick::Attempted {
            host_id,
            config_revision: selected.config_revision(),
            source,
            process_containment_uncertain,
            result,
            next_eligible_in,
            next_wake_in,
        })
    }

    fn monotonic_now(&mut self) -> Duration {
        let observed = self.clock.now();
        if observed > self.last_observed_now {
            self.last_observed_now = observed;
        }
        self.last_observed_now
    }

    fn clear_ineligible_state(&mut self) {
        self.hosts.clear();
        self.last_dispatched_host = None;
        self.last_dispatch_finished_at = None;
    }

    fn reconcile_hosts(
        &mut self,
        eligible: &[RemoteHostConfig],
        executor: &mut impl AutomaticRemoteSyncExecutor,
        now: Duration,
        restore_durable_state: bool,
        durable_process_containment_pauses: Option<&BTreeMap<String, bool>>,
    ) -> Result<(), RemoteSyncError> {
        self.hosts
            .retain(|host_id, _| eligible.iter().any(|host| host.id() == host_id));
        for host in eligible {
            match self.hosts.get_mut(host.id()) {
                Some(schedule) if schedule.configured_host == *host => {
                    if let Some(is_paused) =
                        durable_process_containment_pauses.and_then(|paused| paused.get(host.id()))
                    {
                        if *is_paused {
                            schedule.last_attempt = LastAttempt::ProcessContainmentPause;
                        } else if matches!(
                            schedule.last_attempt,
                            LastAttempt::ProcessContainmentPause
                        ) {
                            schedule.last_attempt = LastAttempt::Never;
                        }
                    }
                }
                Some(schedule) => *schedule = HostSchedule::new(host.clone()),
                None => {
                    let mut schedule = HostSchedule::new(host.clone());
                    if restore_durable_state
                        && let Some(seed) = executor.restore_host_schedule(host)?
                        && host.expected_source() == Some(&seed.source)
                    {
                        schedule.last_attempt = if seed.process_containment_paused {
                            LastAttempt::ProcessContainmentPause
                        } else {
                            LastAttempt::Restored {
                                due_at: now.saturating_add(seed.next_eligible_in),
                                consecutive_failures: seed.consecutive_failures,
                            }
                        };
                        schedule.active_until =
                            seed.active_until_in.map(|delay| now.saturating_add(delay));
                    }
                    if durable_process_containment_pauses
                        .and_then(|paused| paused.get(host.id()))
                        .copied()
                        .unwrap_or(false)
                    {
                        schedule.last_attempt = LastAttempt::ProcessContainmentPause;
                    }
                    self.hosts.insert(host.id().to_owned(), schedule);
                }
            }
        }
        if self
            .last_dispatched_host
            .as_ref()
            .is_some_and(|id| !self.hosts.contains_key(id))
        {
            self.last_dispatched_host = None;
        }
        Ok(())
    }

    fn next_due_host_index(
        &self,
        eligible: &[RemoteHostConfig],
        config: &RemotesConfig,
        now: Duration,
        global_due: Duration,
    ) -> Option<usize> {
        if now < global_due {
            return None;
        }
        let start = self
            .last_dispatched_host
            .as_deref()
            .and_then(|last| eligible.iter().position(|host| host.id() == last))
            .map(|index| (index + 1) % eligible.len())
            .unwrap_or(0);
        (0..eligible.len())
            .map(|offset| (start + offset) % eligible.len())
            .find(|index| {
                self.hosts
                    .get(eligible[*index].id())
                    .and_then(|state| state.due_at(config))
                    .is_some_and(|due_at| due_at <= now)
            })
    }

    fn next_wake_delay(
        &self,
        eligible: &[RemoteHostConfig],
        config: &RemotesConfig,
        now: Duration,
        global_due: Duration,
    ) -> Duration {
        let next_due = eligible
            .iter()
            .filter_map(|host| self.hosts.get(host.id()))
            .filter_map(|state| state.due_at(config))
            .map(|due_at| due_at.max(global_due))
            .min()
            .unwrap_or_else(|| now.saturating_add(configured_idle_delay(config)));
        strictly_positive(next_due.saturating_sub(now))
    }
}

fn automatic_remote_sync_limits() -> RemoteSyncLimits {
    RemoteSyncLimits {
        max_pages: NonZeroUsize::new(AUTOMATIC_REMOTE_SYNC_MAX_PAGES)
            .expect("automatic page limit is non-zero"),
        max_response_bytes: AUTOMATIC_REMOTE_SYNC_MAX_RESPONSE_BYTES,
        exchange_timeout: AUTOMATIC_REMOTE_SYNC_EXCHANGE_TIMEOUT,
    }
}

fn report_requires_immediate_active_polling(report: &RemoteSyncReport) -> bool {
    matches!(
        report.completion,
        RemoteSyncCompletion::Continuation(_) | RemoteSyncCompletion::BootstrapRestarted(_)
    )
}

fn configured_idle_delay(config: &RemotesConfig) -> Duration {
    strictly_positive(Duration::from_secs(config.idle_interval_seconds()))
}

fn strictly_positive(delay: Duration) -> Duration {
    if delay.is_zero() {
        MIN_DISPATCH_SPACING
    } else {
        delay
    }
}

fn retry_delay(host_id: &str, consecutive_failures: u32) -> Duration {
    let index = usize::try_from(consecutive_failures.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(FAILURE_BACKOFF_SECONDS.len() - 1);
    let nominal = FAILURE_BACKOFF_SECONDS[index];
    let jitter_percent =
        JITTER_PERCENT_MIN + stable_jitter(host_id, consecutive_failures) % JITTER_PERCENT_SPAN;
    let jittered = nominal.saturating_mul(jitter_percent).saturating_add(99) / 100;
    Duration::from_secs(jittered.max(1))
}

/// Stable FNV-1a avoids process-random hashing and therefore keeps retry
/// staggering deterministic across restarts without storing scheduler state.
fn stable_jitter(host_id: &str, consecutive_failures: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in host_id
        .as_bytes()
        .iter()
        .copied()
        .chain(consecutive_failures.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io;

    use chrono::Utc;
    use tempfile::tempdir;

    use crate::remote_bandwidth_budget::{
        REMOTE_BANDWIDTH_HARD_LIMIT_BYTES, REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES,
        RemoteBandwidthAdmission, RemoteBandwidthBudgetPausedError, RemoteBandwidthBudgetStore,
        RemoteBandwidthTransferKind,
    };
    use crate::remote_ingest_state::RemoteDeltaNextRequestPosition;
    use crate::remote_protocol::MIN_REMOTE_RESPONSE_ENCODED_BYTES;
    use crate::remote_transport::RemoteTransportError;

    const NODE_A: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NODE_B: &str = "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const NODE_C: &str = "node-cccccccccccccccccccccccccccccccc";
    type TestHost<'a> = (&'a str, &'a str, bool, Option<(&'a str, u64)>);

    #[derive(Debug, Default)]
    struct FakeClock {
        seconds: Cell<u64>,
    }

    impl FakeClock {
        fn set(&self, seconds: u64) {
            self.seconds.set(seconds);
        }

        fn advance(&self, seconds: u64) {
            self.seconds.set(self.seconds.get().saturating_add(seconds));
        }
    }

    impl RemoteSyncSchedulerClock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.seconds.get())
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        calls: Vec<(String, u64, RemoteSyncLimits)>,
        results: VecDeque<Result<RemoteSyncReport, RemoteSyncError>>,
        restored: BTreeMap<String, RemoteSyncScheduleSeed>,
        restore_calls: Vec<String>,
        durable_process_containment: Option<BTreeMap<String, bool>>,
        process_containment_signal: bool,
    }

    impl FakeExecutor {
        fn complete_without_changes() -> Result<RemoteSyncReport, RemoteSyncError> {
            Ok(RemoteSyncReport {
                pages_committed: 0,
                changes_committed: 0,
                live_state_changed: false,
                response_bytes: 0,
                completion: RemoteSyncCompletion::Complete,
            })
        }
    }

    impl AutomaticRemoteSyncExecutor for FakeExecutor {
        fn process_containment_paused_hosts(
            &mut self,
            _hosts: &[RemoteHostConfig],
        ) -> Result<Option<BTreeMap<String, bool>>, RemoteSyncError> {
            Ok(self.durable_process_containment.clone())
        }

        fn take_process_containment_signal(&mut self, _host: &RemoteHostConfig) -> bool {
            std::mem::take(&mut self.process_containment_signal)
        }

        fn restore_host_schedule(
            &mut self,
            host: &RemoteHostConfig,
        ) -> Result<Option<RemoteSyncScheduleSeed>, RemoteSyncError> {
            self.restore_calls.push(host.id().to_owned());
            Ok(self.restored.remove(host.id()))
        }

        fn sync_host(
            &mut self,
            selected: &RemoteSyncHostSnapshot,
            limits: RemoteSyncLimits,
        ) -> Result<RemoteSyncReport, RemoteSyncError> {
            self.calls.push((
                selected.host().id().to_owned(),
                selected.config_revision(),
                limits,
            ));
            self.results
                .pop_front()
                .unwrap_or_else(Self::complete_without_changes)
        }
    }

    fn config(
        revision: u64,
        auto_sync_enabled: bool,
        active: u64,
        idle: u64,
        hosts: &[TestHost<'_>],
    ) -> RemotesConfig {
        let hosts = hosts
            .iter()
            .map(|(id, ssh_host, enabled, source)| {
                let expected_source = source.map(|(node_id, generation)| {
                    serde_json::json!({"nodeId": node_id, "generation": generation})
                });
                serde_json::json!({
                    "id": id,
                    "sshHost": ssh_host,
                    "syncEnabled": enabled,
                    "redactContent": true,
                    "expectedSource": expected_source,
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "configRevision": revision,
            "autoSyncEnabled": auto_sync_enabled,
            "activeIntervalSeconds": active,
            "idleIntervalSeconds": idle,
            "hosts": hosts,
        }))
        .unwrap()
    }

    fn two_automatic_hosts() -> RemotesConfig {
        config(
            7,
            true,
            30,
            120,
            &[
                ("alpha", "alpha-alias", true, Some((NODE_A, 1))),
                ("beta", "beta-alias", true, Some((NODE_B, 1))),
            ],
        )
    }

    fn source(node_id: &str, generation: u64) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: std::num::NonZeroU64::new(generation).unwrap(),
        }
    }

    fn tick_attempt(tick: &RemoteSyncSchedulerTick) -> (&str, Duration, Duration) {
        match tick {
            RemoteSyncSchedulerTick::Attempted {
                host_id,
                next_eligible_in,
                next_wake_in,
                ..
            } => (host_id, *next_wake_in, *next_eligible_in),
            other => panic!("expected one attempt, got {other:?}"),
        }
    }

    #[test]
    fn fail_closed_default_and_global_disable_make_zero_connections() {
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        let tick = scheduler
            .tick(&RemotesConfig::default(), &mut executor)
            .unwrap();

        assert!(matches!(tick, RemoteSyncSchedulerTick::Disabled { .. }));
        assert!(executor.calls.is_empty());
        assert!(tick.next_wake_in() > Duration::ZERO);

        let disabled = config(
            1,
            false,
            30,
            60,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        scheduler.tick(&disabled, &mut executor).unwrap();
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn only_automatic_hosts_are_selected_and_each_tick_starts_at_most_one() {
        let config = config(
            9,
            true,
            30,
            120,
            &[
                ("unpaired", "unpaired", false, None),
                ("disabled", "disabled", false, Some((NODE_C, 1))),
                ("alpha", "alpha", true, Some((NODE_A, 1))),
                ("beta", "beta", true, Some((NODE_B, 1))),
            ],
        );
        let clock = FakeClock::default();
        let mut scheduler = RemoteSyncScheduler::new(clock);
        let mut executor = FakeExecutor::default();

        let first = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&first).0, "alpha");
        assert_eq!(tick_attempt(&first).1, Duration::from_secs(1));
        assert_eq!(tick_attempt(&first).2, Duration::from_secs(120));
        assert_eq!(executor.calls.len(), 1);
        assert_eq!(executor.calls[0].1, 9);

        let waiting = scheduler.tick(&config, &mut executor).unwrap();
        assert!(matches!(waiting, RemoteSyncSchedulerTick::Waiting { .. }));
        assert_eq!(executor.calls.len(), 1);

        scheduler.clock.advance(1);
        let second = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&second).0, "beta");
        assert_eq!(executor.calls.len(), 2);
        assert!(
            executor
                .calls
                .iter()
                .all(|(id, _, _)| id == "alpha" || id == "beta")
        );
    }

    #[test]
    fn round_robin_is_fair_when_all_hosts_remain_due() {
        let config = config(
            2,
            true,
            30,
            60,
            &[
                ("alpha", "alpha", true, Some((NODE_A, 1))),
                ("beta", "beta", true, Some((NODE_B, 1))),
                ("gamma", "gamma", true, Some((NODE_C, 1))),
            ],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        for _ in 0..2 {
            executor.results.push_back(Ok(RemoteSyncReport {
                pages_committed: 1,
                changes_committed: 1,
                live_state_changed: false,
                response_bytes: 100,
                completion: RemoteSyncCompletion::Complete,
            }));
        }

        for expected in ["alpha", "beta", "gamma"] {
            let tick = scheduler.tick(&config, &mut executor).unwrap();
            assert_eq!(tick_attempt(&tick).0, expected);
            scheduler.clock.advance(1);
        }
        assert_eq!(
            executor
                .calls
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn idle_success_waits_longer_than_data_or_continuation() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut idle_scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut idle_executor = FakeExecutor::default();
        idle_executor.results.push_back(Ok(RemoteSyncReport {
            pages_committed: 1,
            changes_committed: 0,
            live_state_changed: false,
            response_bytes: 100,
            completion: RemoteSyncCompletion::Complete,
        }));
        let idle = idle_scheduler.tick(&config, &mut idle_executor).unwrap();
        assert_eq!(tick_attempt(&idle).1, Duration::from_secs(120));
        assert_eq!(tick_attempt(&idle).2, Duration::from_secs(120));

        let mut data_scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut data_executor = FakeExecutor::default();
        data_executor.results.push_back(Ok(RemoteSyncReport {
            pages_committed: 1,
            changes_committed: 1,
            live_state_changed: false,
            response_bytes: 100,
            completion: RemoteSyncCompletion::Complete,
        }));
        let data = data_scheduler.tick(&config, &mut data_executor).unwrap();
        assert_eq!(tick_attempt(&data).1, Duration::from_secs(30));
        assert_eq!(tick_attempt(&data).2, Duration::from_secs(30));

        let mut continuation_scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut continuation_executor = FakeExecutor::default();
        continuation_executor
            .results
            .push_back(Ok(RemoteSyncReport {
                pages_committed: 0,
                changes_committed: 0,
                live_state_changed: false,
                response_bytes: 0,
                completion: RemoteSyncCompletion::Continuation(RemoteDeltaNextRequestPosition {
                    delta_cursor: None,
                    exact_range: None,
                    known_live_revision: None,
                }),
            }));
        let continuation = continuation_scheduler
            .tick(&config, &mut continuation_executor)
            .unwrap();
        assert_eq!(tick_attempt(&continuation).1, Duration::from_secs(30));
        assert_eq!(tick_attempt(&continuation).2, Duration::from_secs(30));
    }

    #[test]
    fn live_only_activity_keeps_active_polling_until_ten_minutes_are_quiet() {
        let config = config(
            1,
            true,
            30,
            300,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor.results.push_back(Ok(RemoteSyncReport {
            pages_committed: 1,
            changes_committed: 0,
            live_state_changed: true,
            response_bytes: 100,
            completion: RemoteSyncCompletion::Complete,
        }));
        for _ in 0..3 {
            executor
                .results
                .push_back(FakeExecutor::complete_without_changes());
        }

        let activity = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&activity).2, Duration::from_secs(30));

        scheduler.clock.advance(30);
        let first_quiet = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&first_quiet).2, Duration::from_secs(30));

        scheduler.clock.advance(569);
        let edge = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&edge).2, Duration::from_secs(1));

        scheduler.clock.advance(1);
        let ten_minutes_quiet = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&ten_minutes_quiet).2, Duration::from_secs(300));
    }

    #[test]
    fn restored_activity_window_survives_process_restart() {
        let config = config(
            1,
            true,
            30,
            300,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor.restored.insert(
            "alpha".to_owned(),
            RemoteSyncScheduleSeed::new(source(NODE_A, 1), Duration::from_secs(30), 0)
                .with_active_until_in(Some(Duration::from_secs(600))),
        );

        let waiting = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(waiting.next_wake_in(), Duration::from_secs(30));
        scheduler.clock.advance(30);
        let still_active = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&still_active).2, Duration::from_secs(30));

        scheduler.clock.advance(570);
        let expired = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&expired).2, Duration::from_secs(300));
    }

    #[test]
    fn failures_back_off_per_host_with_deterministic_bounded_jitter() {
        let failure_config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));

        let first = scheduler.tick(&failure_config, &mut executor).unwrap();
        let first_delay = tick_attempt(&first).2;
        assert!((Duration::from_secs(48)..=Duration::from_secs(72)).contains(&first_delay));

        scheduler.clock.advance(first_delay.as_secs());
        let second = scheduler.tick(&failure_config, &mut executor).unwrap();
        let second_delay = tick_attempt(&second).2;
        assert!((Duration::from_secs(96)..=Duration::from_secs(144)).contains(&second_delay));
        assert!(second_delay > first_delay);

        scheduler.clock.advance(second_delay.as_secs());
        let third = scheduler.tick(&failure_config, &mut executor).unwrap();
        let third_delay = tick_attempt(&third).2;
        assert!((Duration::from_secs(240)..=Duration::from_secs(360)).contains(&third_delay));
        // Failure backoff is intentionally independent of the 120-second idle
        // polling interval in this config.
        assert!(third_delay > Duration::from_secs(120));

        assert_eq!(retry_delay("alpha", 12), retry_delay("alpha", 12));
        for (failure, nominal) in [
            (1, 60),
            (2, 120),
            (3, 300),
            (4, 600),
            (5, 1_800),
            (6, 1_800),
        ] {
            let delay = retry_delay("alpha", failure);
            assert!(
                (Duration::from_secs(nominal * 80 / 100)
                    ..=Duration::from_secs(nominal * 120 / 100))
                    .contains(&delay)
            );
        }

        let other = config(
            1,
            true,
            30,
            120,
            &[
                ("alpha", "alpha", true, Some((NODE_A, 1))),
                ("beta", "beta", true, Some((NODE_B, 1))),
            ],
        );
        scheduler.clock.advance(1);
        let tick = scheduler.tick(&other, &mut executor).unwrap();
        assert_eq!(tick_attempt(&tick).0, "beta");
    }

    #[test]
    fn process_containment_pauses_in_process_until_the_exact_host_changes() {
        let original = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor.results.push_back(Err(RemoteSyncError::Transport(
            RemoteTransportError::Cancelled {
                cleanup_error: Some(io::Error::other("escaped helper")),
            },
        )));

        let paused = scheduler.tick(&original, &mut executor).unwrap();
        assert!(matches!(
            paused,
            RemoteSyncSchedulerTick::Attempted {
                process_containment_uncertain: true,
                next_eligible_in: Duration::MAX,
                ..
            }
        ));
        assert_eq!(executor.calls.len(), 1);

        scheduler.clock.advance(120);
        let same_host_new_global_revision = config(
            2,
            true,
            60,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        assert!(matches!(
            scheduler
                .tick(&same_host_new_global_revision, &mut executor)
                .unwrap(),
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert_eq!(
            executor.calls.len(),
            1,
            "an unrelated config revision unlocked the host"
        );

        let edited_host = config(
            3,
            true,
            60,
            120,
            &[("alpha", "new-alpha", true, Some((NODE_A, 1)))],
        );
        assert!(matches!(
            scheduler.tick(&edited_host, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Attempted {
                process_containment_uncertain: false,
                ..
            }
        ));
        assert_eq!(executor.calls.len(), 2);
    }

    #[test]
    fn durable_containment_pause_blocks_restart_and_manual_clear_is_observed_locally() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor {
            durable_process_containment: Some(BTreeMap::from([("alpha".to_owned(), true)])),
            ..FakeExecutor::default()
        };
        executor.restored.insert(
            "alpha".to_owned(),
            RemoteSyncScheduleSeed::new(source(NODE_A, 1), Duration::ZERO, 0)
                .with_process_containment_paused(true),
        );

        assert!(matches!(
            scheduler.tick(&config, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert!(
            executor.calls.is_empty(),
            "restart opened SSH for a paused host"
        );

        executor.durable_process_containment = Some(BTreeMap::from([("alpha".to_owned(), false)]));
        assert!(matches!(
            scheduler.tick(&config, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Attempted { .. }
        ));
        assert_eq!(
            executor.calls.len(),
            1,
            "manual clear was not observed in-process"
        );
    }

    #[test]
    fn secondary_exchange_containment_pauses_even_when_aggregate_succeeded() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor {
            process_containment_signal: true,
            ..FakeExecutor::default()
        };

        let tick = scheduler.tick(&config, &mut executor).unwrap();
        assert!(matches!(
            tick,
            RemoteSyncSchedulerTick::Attempted {
                result: Ok(_),
                process_containment_uncertain: true,
                next_eligible_in: Duration::MAX,
                ..
            }
        ));
        scheduler.clock.advance(120);
        assert!(matches!(
            scheduler.tick(&config, &mut executor).unwrap(),
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert_eq!(executor.calls.len(), 1);
    }

    #[test]
    fn process_start_restores_exact_source_deadline_and_failure_streak_once() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor.restored.insert(
            "alpha".to_owned(),
            RemoteSyncScheduleSeed::new(source(NODE_A, 1), Duration::from_secs(90), 2),
        );
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));

        let waiting = scheduler.tick(&config, &mut executor).unwrap();
        assert!(matches!(
            waiting,
            RemoteSyncSchedulerTick::Waiting { next_wake_in }
                if next_wake_in == Duration::from_secs(90)
        ));
        assert!(executor.calls.is_empty());
        assert_eq!(executor.restore_calls, ["alpha"]);

        scheduler.clock.advance(90);
        let failed = scheduler.tick(&config, &mut executor).unwrap();
        // The restored streak was two, so this failure uses the third-failure
        // backoff rather than restarting at the first-failure interval.
        assert!(
            (Duration::from_secs(240)..=Duration::from_secs(360))
                .contains(&tick_attempt(&failed).2)
        );
        assert_eq!(executor.calls.len(), 1);
        assert_eq!(executor.restore_calls, ["alpha"]);
    }

    #[test]
    fn successful_attempt_resets_failure_backoff_to_the_first_step() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(FakeExecutor::complete_without_changes());
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));

        let first_failure = scheduler.tick(&config, &mut executor).unwrap();
        scheduler
            .clock
            .advance(tick_attempt(&first_failure).2.as_secs());
        let success = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&success).2, Duration::from_secs(120));

        scheduler.clock.advance(120);
        let failure_after_success = scheduler.tick(&config, &mut executor).unwrap();
        assert!(
            (Duration::from_secs(48)..=Duration::from_secs(72))
                .contains(&tick_attempt(&failure_after_success).2)
        );
    }

    #[test]
    fn source_generation_change_resets_failure_backoff() {
        let initial = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let rotated = config(
            2,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 2)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));

        let first_generation = scheduler.tick(&initial, &mut executor).unwrap();
        assert!(
            (Duration::from_secs(48)..=Duration::from_secs(72))
                .contains(&tick_attempt(&first_generation).2)
        );
        scheduler.clock.advance(1);
        let second_generation = scheduler.tick(&rotated, &mut executor).unwrap();
        assert!(
            (Duration::from_secs(48)..=Duration::from_secs(72))
                .contains(&tick_attempt(&second_generation).2)
        );
    }

    #[test]
    fn process_start_ignores_durable_deadline_for_another_generation() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 2)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor.restored.insert(
            "alpha".to_owned(),
            RemoteSyncScheduleSeed::new(source(NODE_A, 1), Duration::from_secs(90), 4),
        );

        let attempted = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&attempted).0, "alpha");
        assert_eq!(executor.calls.len(), 1);
    }

    #[test]
    fn disable_remove_and_exact_host_changes_are_reconciled_independently() {
        let initial = two_automatic_hosts();
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        scheduler.tick(&initial, &mut executor).unwrap();

        scheduler.clock.advance(1);
        let alpha_disabled = config(
            8,
            true,
            30,
            120,
            &[
                ("alpha", "alpha-alias", false, Some((NODE_A, 1))),
                ("beta", "beta-alias", true, Some((NODE_B, 1))),
            ],
        );
        let beta = scheduler.tick(&alpha_disabled, &mut executor).unwrap();
        assert_eq!(tick_attempt(&beta).0, "beta");

        scheduler.clock.advance(1);
        let beta_retargeted = config(
            9,
            true,
            30,
            120,
            &[("beta", "new-beta-alias", true, Some((NODE_B, 2)))],
        );
        let retargeted = scheduler.tick(&beta_retargeted, &mut executor).unwrap();
        assert_eq!(tick_attempt(&retargeted).0, "beta");
        let RemoteSyncSchedulerTick::Attempted {
            source: Some(source),
            ..
        } = &retargeted
        else {
            panic!("retargeted host must carry its exact source generation");
        };
        assert_eq!(source.node_id.as_str(), NODE_B);
        assert_eq!(source.generation.get(), 2);
        assert_eq!(executor.calls.last().unwrap().1, 9);

        let disabled = config(10, false, 30, 120, &[]);
        scheduler.tick(&disabled, &mut executor).unwrap();
        assert_eq!(executor.calls.len(), 3);
    }

    #[test]
    fn every_returned_delay_is_positive_and_clock_rollback_cannot_retry() {
        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let clock = FakeClock::default();
        clock.set(100);
        let mut scheduler = RemoteSyncScheduler::new(clock);
        let mut executor = FakeExecutor::default();

        let first = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&first).1, Duration::from_secs(120));
        scheduler.clock.set(10);
        let rolled_back = scheduler.tick(&config, &mut executor).unwrap();
        assert!(matches!(
            rolled_back,
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert_eq!(rolled_back.next_wake_in(), Duration::from_secs(120));
        assert_eq!(executor.calls.len(), 1);
        assert!(rolled_back.next_wake_in() > Duration::ZERO);
    }

    #[test]
    fn automatic_limits_are_lower_than_manual_defaults() {
        let automatic = automatic_remote_sync_limits();
        let manual = RemoteSyncLimits::default();
        assert_eq!(automatic.max_pages.get(), 1);
        assert!(automatic.max_pages < manual.max_pages);
        assert!(automatic.max_response_bytes < manual.max_response_bytes);
        assert!(automatic.exchange_timeout < manual.exchange_timeout);
        assert!(automatic.max_response_bytes >= MIN_REMOTE_RESPONSE_ENCODED_BYTES + 20);
        assert_eq!(
            automatic.max_response_bytes,
            MAX_REMOTE_FRAME_ENCODED_BYTES + 20
        );
    }

    #[test]
    fn typed_bandwidth_pause_uses_idle_interval_instead_of_failure_backoff() {
        let directory = tempdir().unwrap();
        let budget = RemoteBandwidthBudgetStore::new(directory.path().join("state"));
        let node: NodeId = NODE_A.parse().unwrap();
        let now = Utc::now();
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve");
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(REMOTE_BANDWIDTH_SOFT_LIMIT_BYTES).unwrap(),
            )
            .unwrap();
        let RemoteBandwidthAdmission::Paused(pause) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::AutomaticBulk,
                1024,
            )
            .unwrap()
        else {
            panic!("bulk must pause at the soft cap");
        };

        let config = config(
            1,
            true,
            30,
            120,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::Local(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                RemoteBandwidthBudgetPausedError::new(pause),
            ))));

        let tick = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(tick_attempt(&tick).2, Duration::from_secs(120));
    }

    #[test]
    fn hard_bandwidth_pause_caps_long_idle_and_restart_seed_does_not_probe_early() {
        let directory = tempdir().unwrap();
        let budget = RemoteBandwidthBudgetStore::new(directory.path().join("state"));
        let node: NodeId = NODE_A.parse().unwrap();
        let now = Utc::now();
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve");
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(REMOTE_BANDWIDTH_HARD_LIMIT_BYTES).unwrap(),
            )
            .unwrap();
        let RemoteBandwidthAdmission::Paused(pause) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::AutomaticIncremental,
                1024,
            )
            .unwrap()
        else {
            panic!("incremental sync must pause at the hard cap");
        };

        let config = config(
            1,
            true,
            30,
            24 * 60 * 60,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::Local(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                RemoteBandwidthBudgetPausedError::new(pause),
            ))));
        let tick = scheduler.tick(&config, &mut executor).unwrap();
        let probe_interval = Duration::from_secs(
            u64::try_from(REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS).unwrap(),
        );
        assert_eq!(tick_attempt(&tick).2, probe_interval);

        let mut restarted = RemoteSyncScheduler::new(FakeClock::default());
        let mut restarted_executor = FakeExecutor::default();
        restarted_executor.restored.insert(
            "alpha".to_owned(),
            RemoteSyncScheduleSeed::new(source(NODE_A, 1), probe_interval, 0),
        );
        let waiting = restarted.tick(&config, &mut restarted_executor).unwrap();
        assert!(matches!(waiting, RemoteSyncSchedulerTick::Waiting { .. }));
        assert_eq!(waiting.next_wake_in(), probe_interval);
        assert!(restarted_executor.calls.is_empty());

        restarted.clock.advance(probe_interval.as_secs() - 1);
        let still_waiting = restarted.tick(&config, &mut restarted_executor).unwrap();
        assert!(matches!(
            still_waiting,
            RemoteSyncSchedulerTick::Waiting { .. }
        ));
        assert!(restarted_executor.calls.is_empty());
        restarted.clock.advance(1);
        let due = restarted.tick(&config, &mut restarted_executor).unwrap();
        assert!(matches!(due, RemoteSyncSchedulerTick::Attempted { .. }));
        assert_eq!(restarted_executor.calls.len(), 1);
    }

    #[test]
    fn budget_pause_preserves_the_process_local_failure_streak() {
        let directory = tempdir().unwrap();
        let budget = RemoteBandwidthBudgetStore::new(directory.path().join("state"));
        let node: NodeId = NODE_A.parse().unwrap();
        let now = Utc::now();
        let RemoteBandwidthAdmission::Granted(reservation) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::ManualOverride,
                1,
            )
            .unwrap()
        else {
            panic!("manual override must reserve");
        };
        budget
            .complete_attempt(
                &reservation,
                now,
                usize::try_from(REMOTE_BANDWIDTH_HARD_LIMIT_BYTES).unwrap(),
            )
            .unwrap();
        let RemoteBandwidthAdmission::Paused(pause) = budget
            .begin_attempt(
                "alpha",
                Some(&node),
                now,
                RemoteBandwidthTransferKind::AutomaticIncremental,
                1024,
            )
            .unwrap()
        else {
            panic!("incremental sync must pause at the hard cap");
        };

        let config = config(
            1,
            true,
            30,
            24 * 60 * 60,
            &[("alpha", "alpha", true, Some((NODE_A, 1)))],
        );
        let mut scheduler = RemoteSyncScheduler::new(FakeClock::default());
        let mut executor = FakeExecutor::default();
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));
        executor
            .results
            .push_back(Err(RemoteSyncError::Local(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                RemoteBandwidthBudgetPausedError::new(pause),
            ))));
        executor
            .results
            .push_back(Err(RemoteSyncError::InvalidStartedAt));

        let first_failure = scheduler.tick(&config, &mut executor).unwrap();
        scheduler
            .clock
            .advance(tick_attempt(&first_failure).2.as_secs());
        let second_failure = scheduler.tick(&config, &mut executor).unwrap();
        scheduler
            .clock
            .advance(tick_attempt(&second_failure).2.as_secs());
        let paused = scheduler.tick(&config, &mut executor).unwrap();
        assert_eq!(
            tick_attempt(&paused).2,
            Duration::from_secs(
                u64::try_from(REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS).unwrap()
            )
        );
        scheduler.clock.advance(tick_attempt(&paused).2.as_secs());
        let failure_after_pause = scheduler.tick(&config, &mut executor).unwrap();
        assert!(
            (Duration::from_secs(240)..=Duration::from_secs(360))
                .contains(&tick_attempt(&failure_after_pause).2),
            "the failure after a budget pause must use the third-failure backoff"
        );
    }
}
