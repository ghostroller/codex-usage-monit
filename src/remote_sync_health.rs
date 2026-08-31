//! Durable, sanitized health state for remote synchronization.
//!
//! This store is deliberately independent from transport and configuration
//! discovery. Reading it only opens local files below the supplied state root;
//! it can never enumerate SSH configuration or connect to a remote machine.
//! The wire format contains stable host IDs and optional complete source
//! generation pins, but never SSH aliases, host names, paths, raw errors, or
//! message text.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;
use crate::remote_bandwidth_budget::RemoteBandwidthBudgetLevel;
use crate::remote_fact_sync::{RemoteFactSyncError, ReplicaFactCandidateKey};
use crate::remote_protocol::SourceGeneration;
use crate::remote_sync::{RemoteSyncCompletion, RemoteSyncError, RemoteSyncReport};
use crate::remotes_config::{RemoteHostConfig, RemotesConfig};
use crate::source_history::RedactionProfile;
use crate::source_identity::NodeId;
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

pub const REMOTE_SYNC_HEALTH_SCHEMA_VERSION: u32 = 3;
pub const REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS: i64 = 30 * 60;

const HEALTH_DIRECTORY: &str = "remote-sync-health-v3";
const HEALTH_FILE: &str = "health.json";
const HEALTH_LOCK_FILE: &str = "health.lock";
// Exact fact cooldown keys include two SHA-256 bindings and all estimator
// revisions so fresh digest evidence is never hidden by a stale cooldown.
// Keep the file bound above the validated 1,024-entry worst case while still
// capping startup memory to a small fixed amount.
const MAX_HEALTH_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_HEALTH_HOSTS: usize = 512;
const MAX_FACT_RESOURCE_COOLDOWNS: usize = 1_024;
const MAX_FACT_RESOURCE_COOLDOWNS_PER_HOST: usize = 32;
const MAX_FACT_RESOURCE_RESUME_CURSORS_PER_HOST: usize = 2;
const FACT_EVIDENCE_RETRY_DELAY_MINUTES: i64 = 15;
const FACT_RESOURCE_RETRY_DELAY_HOURS: i64 = 6;
const MAX_HOST_ID_BYTES: usize = 64;
const REMOTE_HOST_FINGERPRINT_PREFIX: &str = "remote-host-sha256-v1-";
const REMOTE_HOST_FINGERPRINT_HEX_BYTES: usize = 64;
const MAX_PAGES_PER_ATTEMPT: u32 = 32;
const MAX_RESPONSE_BYTES_PER_ATTEMPT: u64 = 128 * 1024 * 1024;
const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Sanitized outcome of the latest attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncAttemptResult {
    Success,
    Failure,
}

/// Sanitized pagination state of a successful attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncHealthCompletion {
    Complete,
    Continuation,
    BootstrapRestarted,
}

impl From<&RemoteSyncCompletion> for RemoteSyncHealthCompletion {
    fn from(completion: &RemoteSyncCompletion) -> Self {
        match completion {
            RemoteSyncCompletion::Complete => Self::Complete,
            RemoteSyncCompletion::Continuation(_) => Self::Continuation,
            RemoteSyncCompletion::BootstrapRestarted(_) => Self::BootstrapRestarted,
        }
    }
}

/// Bounded error taxonomy. No variant carries caller-controlled text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncErrorCategory {
    Configuration,
    Policy,
    Busy,
    ResourceLimit,
    LocalState,
    Protocol,
    ProcessContainment,
    Transport,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteFactResourceCooldown {
    redaction_profile: RedactionProfile,
    candidate: ReplicaFactCandidateKey,
    retry_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteFactResourceResumeCursor {
    redaction_profile: RedactionProfile,
    candidate: ReplicaFactCandidateKey,
}

impl RemoteFactResourceCooldown {
    fn candidate_key(&self) -> ReplicaFactCandidateKey {
        self.candidate.clone()
    }

    fn matches(
        &self,
        redaction_profile: RedactionProfile,
        candidate: &ReplicaFactCandidateKey,
    ) -> bool {
        self.redaction_profile == redaction_profile && self.candidate == *candidate
    }
}

impl RemoteSyncErrorCategory {
    /// Classifies a sync error without retaining or serializing its message.
    pub fn from_sync_error(error: &RemoteSyncError) -> Self {
        match error {
            RemoteSyncError::HostNotPaired { .. }
            | RemoteSyncError::HostNotEnabledForAutomaticSync { .. }
            | RemoteSyncError::StaleHostSelection { .. }
            | RemoteSyncError::ConfigurationChanged { .. } => Self::Configuration,
            RemoteSyncError::InvalidLimits(_)
            | RemoteSyncError::InvalidStartedAt
            | RemoteSyncError::ResponseBudgetExceeded => Self::ResourceLimit,
            RemoteSyncError::UnboundResponseEnvelope
            | RemoteSyncError::UnexpectedResponse
            | RemoteSyncError::Protocol(_) => Self::Protocol,
            RemoteSyncError::ProcessContainment => Self::ProcessContainment,
            RemoteSyncError::PreTransportLocal(error) | RemoteSyncError::Local(error) => {
                match error.kind() {
                    io::ErrorKind::PermissionDenied => Self::Policy,
                    io::ErrorKind::WouldBlock => Self::Busy,
                    io::ErrorKind::InvalidData => Self::Protocol,
                    _ => Self::LocalState,
                }
            }
            RemoteSyncError::Transport(error) if error.process_containment_uncertain() => {
                Self::ProcessContainment
            }
            RemoteSyncError::Transport(_) => Self::Transport,
            RemoteSyncError::Remote(_) => Self::Remote,
        }
    }

    /// Classifies a per-thread fact follow-up independently from the already
    /// committed aggregate synchronization.
    pub fn from_fact_sync_error(error: &RemoteFactSyncError) -> Self {
        match error {
            RemoteFactSyncError::HostNotPaired { .. }
            | RemoteFactSyncError::ConfigurationChanged { .. } => Self::Configuration,
            RemoteFactSyncError::InvalidLimits(_)
            | RemoteFactSyncError::InvalidRetentionDays(_)
            | RemoteFactSyncError::ResponseBudgetExceeded
            | RemoteFactSyncError::DecodedBudgetExceeded
            | RemoteFactSyncError::RecordBudgetExceeded
            | RemoteFactSyncError::RunBudgetExhausted => Self::ResourceLimit,
            RemoteFactSyncError::UnboundResponseEnvelope
            | RemoteFactSyncError::UnexpectedResponse
            | RemoteFactSyncError::PageContinuity(_)
            | RemoteFactSyncError::Protocol(_) => Self::Protocol,
            RemoteFactSyncError::PreTransportLocal(error) | RemoteFactSyncError::Local(error) => {
                match error.kind() {
                    io::ErrorKind::PermissionDenied => Self::Policy,
                    io::ErrorKind::WouldBlock => Self::Busy,
                    _ => Self::LocalState,
                }
            }
            RemoteFactSyncError::Transport(error) if error.process_containment_uncertain() => {
                Self::ProcessContainment
            }
            RemoteFactSyncError::Transport(_) => Self::Transport,
            RemoteFactSyncError::Remote(_) => Self::Remote,
        }
    }
}

/// Validated, read-only view of one host's latest local sync health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSyncHostHealth {
    host_id: String,
    configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<SourceGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_success_at: Option<DateTime<Utc>>,
    /// Latest successful aggregate attempt that committed either historical
    /// changes or a full semantic live-state replacement. Idle successes and
    /// failures preserve this timestamp for scheduler restart hysteresis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_activity_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_result: Option<RemoteSyncAttemptResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion: Option<RemoteSyncHealthCompletion>,
    pages_committed: u32,
    changes_committed: u64,
    response_bytes: u64,
    consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_eligible_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_category: Option<RemoteSyncErrorCategory>,
    /// An automatic transport attempt could not prove that its complete SSH
    /// process tree was reclaimed. This pause is independent from ordinary
    /// retry backoff and is bound to a content-free fingerprint of the exact
    /// host row which authorized the attempt; unrelated config edits cannot
    /// unlock it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_containment_paused_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_containment_host_fingerprint: Option<String>,
    /// Latest bounded fact-follow-up outcome for this exact source pin.
    /// `None` means no fact follow-up has completed yet; a timestamp with no
    /// category is healthy, while a category is a sanitized partial/attention
    /// signal. Aggregate success fields remain independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_fact_sync_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fact_sync_error_category: Option<RemoteSyncErrorCategory>,
    /// Exact content-free candidates whose remote complete inventory exceeded
    /// the protocol bound. This prevents one oversized newest digest from
    /// starving other candidates while retaining a visible attention state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_resource_cooldowns: Vec<RemoteFactResourceCooldown>,
    /// Per-profile fair-scan cursor. Unlike the bounded cooldown inventory,
    /// this single exact key lets planning resume after the last failed
    /// candidate even when that candidate could not fit in the inventory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fact_resource_resume_cursors: Vec<RemoteFactResourceResumeCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_paused_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_resume_at: Option<DateTime<Utc>>,
    /// Durable claim deadline for the next fixed-size automatic hard-cap
    /// probe. `None` means either a soft pause or a legacy/new pause which has
    /// not yet been classified by the bandwidth policy observer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_probe_due_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_last_probe_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_last_probe_succeeded: Option<bool>,
}

impl RemoteSyncHostHealth {
    fn new(host_id: String, configured: bool, source: Option<SourceGeneration>) -> Self {
        Self {
            host_id,
            configured,
            source,
            last_attempt_at: None,
            last_success_at: None,
            last_activity_at: None,
            last_result: None,
            completion: None,
            pages_committed: 0,
            changes_committed: 0,
            response_bytes: 0,
            consecutive_failures: 0,
            next_eligible_at: None,
            error_category: None,
            process_containment_paused_at: None,
            process_containment_host_fingerprint: None,
            last_fact_sync_at: None,
            fact_sync_error_category: None,
            fact_resource_cooldowns: Vec::new(),
            fact_resource_resume_cursors: Vec::new(),
            budget_paused_at: None,
            budget_resume_at: None,
            budget_probe_due_at: None,
            budget_last_probe_at: None,
            budget_last_probe_succeeded: None,
        }
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn configured(&self) -> bool {
        self.configured
    }

    pub fn source(&self) -> Option<&SourceGeneration> {
        self.source.as_ref()
    }

    /// Convenience for callers that only need to group health by stable node.
    /// Generation-sensitive decisions must use [`Self::source`].
    pub fn node_id(&self) -> Option<&NodeId> {
        self.source.as_ref().map(|source| &source.node_id)
    }

    pub fn last_attempt_at(&self) -> Option<DateTime<Utc>> {
        self.last_attempt_at
    }

    pub fn last_success_at(&self) -> Option<DateTime<Utc>> {
        self.last_success_at
    }

    pub fn last_activity_at(&self) -> Option<DateTime<Utc>> {
        self.last_activity_at
    }

    pub fn last_result(&self) -> Option<RemoteSyncAttemptResult> {
        self.last_result
    }

    pub fn completion(&self) -> Option<RemoteSyncHealthCompletion> {
        self.completion
    }

    pub fn pages_committed(&self) -> u32 {
        self.pages_committed
    }

    pub fn changes_committed(&self) -> u64 {
        self.changes_committed
    }

    pub fn response_bytes(&self) -> u64 {
        self.response_bytes
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn next_eligible_at(&self) -> Option<DateTime<Utc>> {
        self.next_eligible_at
    }

    pub fn error_category(&self) -> Option<RemoteSyncErrorCategory> {
        self.error_category
    }

    pub fn process_containment_paused_at(&self) -> Option<DateTime<Utc>> {
        self.process_containment_paused_at
    }

    pub fn process_containment_paused(&self) -> bool {
        self.process_containment_paused_at.is_some()
    }

    pub fn process_containment_host_fingerprint(&self) -> Option<&str> {
        self.process_containment_host_fingerprint.as_deref()
    }

    pub fn process_containment_paused_for(&self, host: &RemoteHostConfig) -> bool {
        self.process_containment_paused_at.is_some()
            && self.process_containment_host_fingerprint.as_deref()
                == Some(host.automatic_sync_fingerprint().as_str())
    }

    pub fn last_fact_sync_at(&self) -> Option<DateTime<Utc>> {
        self.last_fact_sync_at
    }

    pub fn fact_sync_error_category(&self) -> Option<RemoteSyncErrorCategory> {
        self.fact_sync_error_category
    }

    pub fn fact_sync_needs_attention(&self) -> bool {
        self.fact_sync_error_category.is_some()
    }

    /// Time at which the latest independent bandwidth pause was observed.
    /// A pause is a local policy state, not a failed synchronization attempt.
    pub fn budget_paused_at(&self) -> Option<DateTime<Utc>> {
        self.budget_paused_at
    }

    /// Exact UTC instant at which the recorded budget pause may be retried.
    /// When [`Self::budget_paused`] is true, `None` means the recovery time is
    /// unknown rather than that the host is unpaused.
    pub fn budget_resume_at(&self) -> Option<DateTime<Utc>> {
        self.budget_resume_at
    }

    pub fn budget_paused(&self) -> bool {
        self.budget_paused_at.is_some()
    }

    pub fn budget_probe_due_at(&self) -> Option<DateTime<Utc>> {
        self.budget_probe_due_at
    }

    pub fn budget_last_probe_at(&self) -> Option<DateTime<Utc>> {
        self.budget_last_probe_at
    }

    pub fn budget_last_probe_succeeded(&self) -> Option<bool> {
        self.budget_last_probe_succeeded
    }

    fn validate(&self) -> io::Result<()> {
        validate_stored_host_id(&self.host_id)?;
        if self.pages_committed > MAX_PAGES_PER_ATTEMPT {
            return Err(invalid_data(
                "remote sync health pagesCommitted exceeds the supported limit",
            ));
        }
        if self.response_bytes > MAX_RESPONSE_BYTES_PER_ATTEMPT {
            return Err(invalid_data(
                "remote sync health responseBytes exceeds the supported limit",
            ));
        }
        if let (Some(last_success), Some(last_attempt)) =
            (self.last_success_at, self.last_attempt_at)
            && last_success > last_attempt
        {
            return Err(invalid_data(
                "remote sync health lastSuccessAt cannot follow lastAttemptAt",
            ));
        }
        if let (Some(last_activity), Some(last_success)) =
            (self.last_activity_at, self.last_success_at)
            && last_activity > last_success
        {
            return Err(invalid_data(
                "remote sync health lastActivityAt cannot follow lastSuccessAt",
            ));
        }
        if self.last_activity_at.is_some() && self.last_success_at.is_none() {
            return Err(invalid_data(
                "remote sync health activity requires a successful attempt",
            ));
        }
        if let (Some(next_eligible), Some(last_attempt)) =
            (self.next_eligible_at, self.last_attempt_at)
            && next_eligible < last_attempt
        {
            return Err(invalid_data(
                "remote sync health nextEligibleAt cannot precede lastAttemptAt",
            ));
        }
        match (
            self.process_containment_paused_at,
            self.process_containment_host_fingerprint.as_deref(),
        ) {
            (None, None) => {}
            (Some(_), Some(fingerprint))
                if self.source.is_some() && valid_remote_host_fingerprint(fingerprint) => {}
            (Some(_), Some(_)) => {
                return Err(invalid_data(
                    "remote process-containment pause requires an exact source pin and valid host fingerprint",
                ));
            }
            _ => {
                return Err(invalid_data(
                    "remote process-containment pause timestamp and host fingerprint must be present together",
                ));
            }
        }
        if self.fact_sync_error_category.is_some() && self.last_fact_sync_at.is_none() {
            return Err(invalid_data(
                "remote fact sync attention requires a fact sync timestamp",
            ));
        }
        if self.last_fact_sync_at.is_some() && self.source.is_none() {
            return Err(invalid_data(
                "remote fact sync health requires an exact source pin",
            ));
        }
        if self.fact_resource_cooldowns.len() > MAX_FACT_RESOURCE_COOLDOWNS_PER_HOST {
            return Err(invalid_data(
                "remote fact resource cooldown count exceeds the per-host bound",
            ));
        }
        if self.fact_resource_resume_cursors.len() > MAX_FACT_RESOURCE_RESUME_CURSORS_PER_HOST {
            return Err(invalid_data(
                "remote fact resource resume cursor count exceeds the per-host bound",
            ));
        }
        if !self.fact_resource_cooldowns.is_empty() && self.source.is_none() {
            return Err(invalid_data(
                "remote fact resource cooldowns require an exact source pin",
            ));
        }
        if !self.fact_resource_resume_cursors.is_empty() && self.source.is_none() {
            return Err(invalid_data(
                "remote fact resource resume cursors require an exact source pin",
            ));
        }
        let mut cooldown_keys = HashSet::with_capacity(self.fact_resource_cooldowns.len());
        for cooldown in &self.fact_resource_cooldowns {
            cooldown.candidate.validate()?;
            if !cooldown_keys.insert((cooldown.redaction_profile, cooldown.candidate.clone())) {
                return Err(invalid_data(
                    "remote fact resource cooldowns contain a duplicate candidate",
                ));
            }
        }
        let mut resume_profiles = HashSet::with_capacity(self.fact_resource_resume_cursors.len());
        for cursor in &self.fact_resource_resume_cursors {
            cursor.candidate.validate()?;
            if !resume_profiles.insert(cursor.redaction_profile) {
                return Err(invalid_data(
                    "remote fact resource resume cursors contain a duplicate profile",
                ));
            }
        }
        match (self.budget_paused_at, self.budget_resume_at) {
            (None, None) => {}
            (Some(_), None) => {}
            (Some(paused_at), Some(resume_at)) if resume_at >= paused_at => {}
            (Some(_), Some(_)) => {
                return Err(invalid_data(
                    "remote sync health budgetResumeAt cannot precede budgetPausedAt",
                ));
            }
            (None, Some(_)) => {
                return Err(invalid_data(
                    "remote sync health budgetResumeAt requires budgetPausedAt",
                ));
            }
        }
        if self.budget_paused_at.is_none() && self.budget_probe_due_at.is_some() {
            return Err(invalid_data(
                "remote sync health budget probe deadline requires a budget pause",
            ));
        }
        if self.budget_paused_at.is_none()
            && (self.budget_last_probe_at.is_some() || self.budget_last_probe_succeeded.is_some())
        {
            return Err(invalid_data(
                "remote sync health budget probe result requires a budget pause",
            ));
        }
        if self.budget_last_probe_succeeded.is_some() && self.budget_last_probe_at.is_none() {
            return Err(invalid_data(
                "remote sync health budget probe outcome requires a probe timestamp",
            ));
        }
        if let (Some(paused_at), Some(probed_at)) =
            (self.budget_paused_at, self.budget_last_probe_at)
            && probed_at < paused_at
        {
            return Err(invalid_data(
                "remote sync health budget probe cannot precede its pause",
            ));
        }
        if let (Some(probed_at), Some(probe_due_at)) =
            (self.budget_last_probe_at, self.budget_probe_due_at)
            && probe_due_at <= probed_at
        {
            return Err(invalid_data(
                "remote sync health next budget probe must follow the last claim",
            ));
        }
        if let (Some(paused_at), Some(probe_due_at)) =
            (self.budget_paused_at, self.budget_probe_due_at)
            && probe_due_at < paused_at
        {
            return Err(invalid_data(
                "remote sync health budget probe deadline cannot precede its pause",
            ));
        }

        match (self.last_attempt_at, self.last_result) {
            (None, None) => {
                if self.last_success_at.is_some()
                    || self.last_activity_at.is_some()
                    || self.completion.is_some()
                    || self.pages_committed != 0
                    || self.changes_committed != 0
                    || self.response_bytes != 0
                    || self.consecutive_failures != 0
                    || self.next_eligible_at.is_some()
                    || self.error_category.is_some()
                {
                    return Err(invalid_data(
                        "remote sync health without an attempt contains attempt details",
                    ));
                }
            }
            (Some(last_attempt), Some(RemoteSyncAttemptResult::Success)) => {
                if self.last_success_at != Some(last_attempt)
                    || self.completion.is_none()
                    || self.consecutive_failures != 0
                    || self.error_category.is_some()
                {
                    return Err(invalid_data(
                        "successful remote sync health has inconsistent fields",
                    ));
                }
            }
            (Some(_), Some(RemoteSyncAttemptResult::Failure)) => {
                if self.completion.is_some()
                    || self.pages_committed != 0
                    || self.changes_committed != 0
                    || self.response_bytes != 0
                    || self.consecutive_failures == 0
                    || self.error_category.is_none()
                {
                    return Err(invalid_data(
                        "failed remote sync health has inconsistent fields",
                    ));
                }
            }
            _ => {
                return Err(invalid_data(
                    "remote sync health attempt timestamp/result must be present together",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteSyncHealth {
    schema_version: u32,
    hosts: Vec<RemoteSyncHostHealth>,
}

impl Default for StoredRemoteSyncHealth {
    fn default() -> Self {
        Self {
            schema_version: REMOTE_SYNC_HEALTH_SCHEMA_VERSION,
            hosts: Vec::new(),
        }
    }
}

impl StoredRemoteSyncHealth {
    fn validate(&self) -> io::Result<()> {
        if self.schema_version != REMOTE_SYNC_HEALTH_SCHEMA_VERSION {
            let relation = if self.schema_version > REMOTE_SYNC_HEALTH_SCHEMA_VERSION {
                "future"
            } else {
                "unsupported"
            };
            return Err(invalid_data(format!(
                "{relation} remote sync health schema version {}; expected {}",
                self.schema_version, REMOTE_SYNC_HEALTH_SCHEMA_VERSION
            )));
        }
        if self.hosts.len() > MAX_HEALTH_HOSTS {
            return Err(invalid_data(format!(
                "remote sync health has {} hosts; maximum is {MAX_HEALTH_HOSTS}",
                self.hosts.len()
            )));
        }
        let mut ids = HashSet::with_capacity(self.hosts.len());
        let mut previous = None;
        let mut fact_resource_cooldowns = 0usize;
        for host in &self.hosts {
            host.validate()?;
            fact_resource_cooldowns = fact_resource_cooldowns
                .checked_add(host.fact_resource_cooldowns.len())
                .ok_or_else(|| invalid_data("remote fact resource cooldown count overflowed"))?;
            if !ids.insert(host.host_id.as_str()) {
                return Err(invalid_data(
                    "remote sync health contains a duplicate host ID",
                ));
            }
            if previous.is_some_and(|previous: &str| previous >= host.host_id.as_str()) {
                return Err(invalid_data(
                    "remote sync health hosts must be sorted by host ID",
                ));
            }
            previous = Some(host.host_id.as_str());
        }
        if fact_resource_cooldowns > MAX_FACT_RESOURCE_COOLDOWNS {
            return Err(invalid_data(
                "remote fact resource cooldown count exceeds the global bound",
            ));
        }
        Ok(())
    }

    fn into_map(self) -> BTreeMap<String, RemoteSyncHostHealth> {
        self.hosts
            .into_iter()
            .map(|host| (host.host_id.clone(), host))
            .collect()
    }

    fn from_map(hosts: BTreeMap<String, RemoteSyncHostHealth>) -> Self {
        Self {
            schema_version: REMOTE_SYNC_HEALTH_SCHEMA_VERSION,
            hosts: hosts.into_values().collect(),
        }
    }
}

/// Private, atomically updated health store rooted beside history state.
#[derive(Clone, Debug)]
pub struct RemoteSyncHealthStore {
    state_root: PathBuf,
}

impl RemoteSyncHealthStore {
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Lists validated records in stable host-ID order. No missing file is an
    /// empty state, and this read path performs no network operation.
    pub fn list(&self) -> io::Result<Vec<RemoteSyncHostHealth>> {
        Ok(self.load()?.hosts)
    }

    /// Reads one validated host record without contacting the host.
    pub fn get(&self, host_id: &str) -> io::Result<Option<RemoteSyncHostHealth>> {
        validate_host_id(host_id)?;
        Ok(self
            .load()?
            .hosts
            .into_iter()
            .find(|host| host.host_id == host_id))
    }

    /// Records the independent fact-follow-up outcome without rewriting the
    /// aggregate attempt. This is intentionally a latest-value signal: a late
    /// completion cannot roll a newer success or attention state backwards.
    pub fn record_fact_sync_outcome(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        completed_at: DateTime<Utc>,
        error_category: Option<RemoteSyncErrorCategory>,
    ) -> io::Result<RemoteSyncHostHealth> {
        self.record_fact_sync_outcome_inner(host_id, source, completed_at, error_category, None)
    }

    /// Atomically stores a fact-follow-up containment outcome and pauses the
    /// exact configured host before any later local bookkeeping can fail.
    pub fn record_fact_sync_process_containment(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        completed_at: DateTime<Utc>,
        configured_host: &RemoteHostConfig,
    ) -> io::Result<RemoteSyncHostHealth> {
        if configured_host.id() != host_id || configured_host.expected_source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote fact containment outcome does not match its configured host pin",
            ));
        }
        self.record_fact_sync_outcome_inner(
            host_id,
            source,
            completed_at,
            Some(RemoteSyncErrorCategory::ProcessContainment),
            Some(configured_host.automatic_sync_fingerprint()),
        )
    }

    fn record_fact_sync_outcome_inner(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        completed_at: DateTime<Utc>,
        error_category: Option<RemoteSyncErrorCategory>,
        process_containment_host_fingerprint: Option<String>,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, Some(source))?;
            if host
                .last_fact_sync_at
                .is_some_and(|current| completed_at < current)
            {
                return Ok(host.clone());
            }
            host.configured = true;
            host.last_fact_sync_at = Some(completed_at);
            host.fact_sync_error_category = error_category;
            if let Some(host_fingerprint) = process_containment_host_fingerprint {
                host.process_containment_paused_at = Some(completed_at);
                host.process_containment_host_fingerprint = Some(host_fingerprint);
            }
            Ok(host.clone())
        })
    }

    /// Returns only still-active, exact per-replica candidate suppressions for
    /// this source generation/profile. This is a local read and never opens
    /// SSH or exposes thread IDs through diagnostics.
    pub(crate) fn active_fact_resource_exclusions(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        redaction_profile: RedactionProfile,
        now: DateTime<Utc>,
    ) -> io::Result<BTreeSet<ReplicaFactCandidateKey>> {
        validate_host_id(host_id)?;
        let Some(host) = self.get(host_id)? else {
            return Ok(BTreeSet::new());
        };
        if host.source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote fact resource cooldown source pin does not match the selected host",
            ));
        }
        Ok(host
            .fact_resource_cooldowns
            .iter()
            .filter(|cooldown| {
                cooldown.redaction_profile == redaction_profile && now < cooldown.retry_at
            })
            .map(RemoteFactResourceCooldown::candidate_key)
            .collect())
    }

    pub(crate) fn fact_resource_resume_after(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<ReplicaFactCandidateKey>> {
        validate_host_id(host_id)?;
        let Some(host) = self.get(host_id)? else {
            return Ok(None);
        };
        if host.source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote fact resume cursor source pin does not match the selected host",
            ));
        }
        Ok(host
            .fact_resource_resume_cursors
            .iter()
            .find(|cursor| cursor.redaction_profile == redaction_profile)
            .map(|cursor| cursor.candidate.clone()))
    }

    pub(crate) fn clear_fact_resource_resume_after(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        redaction_profile: RedactionProfile,
        expected: Option<&ReplicaFactCandidateKey>,
    ) -> io::Result<()> {
        validate_host_id(host_id)?;
        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, Some(source))?;
            host.fact_resource_resume_cursors.retain(|cursor| {
                cursor.redaction_profile != redaction_profile
                    || expected.is_some_and(|expected| expected != &cursor.candidate)
            });
            Ok(())
        })
    }

    /// Persists a bounded cooldown for one deterministic fact candidate
    /// failure. The candidate may belong to the center-local source or the
    /// selected remote; the enclosing host generation remains the durable
    /// scheduling fence. Expired and oldest entries are pruned before
    /// insertion so one candidate cannot permanently block later replica
    /// work.
    pub(crate) fn record_fact_resource_cooldown(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        redaction_profile: RedactionProfile,
        candidate: &ReplicaFactCandidateKey,
        observed_at: DateTime<Utc>,
        resource_limit: bool,
    ) -> io::Result<()> {
        validate_host_id(host_id)?;
        let retry_delay = if resource_limit {
            TimeDelta::hours(FACT_RESOURCE_RETRY_DELAY_HOURS)
        } else {
            TimeDelta::minutes(FACT_EVIDENCE_RETRY_DELAY_MINUTES)
        };
        let retry_at = observed_at
            .checked_add_signed(retry_delay)
            .ok_or_else(|| invalid_data("remote fact resource cooldown timestamp overflows"))?;
        self.mutate(|hosts| {
            for host in hosts.values_mut() {
                host.fact_resource_cooldowns
                    .retain(|cooldown| observed_at < cooldown.retry_at);
            }

            let already_present = hosts.get(host_id).is_some_and(|host| {
                host.fact_resource_cooldowns
                    .iter()
                    .any(|cooldown| cooldown.matches(redaction_profile, candidate))
            });
            let total = hosts
                .values()
                .map(|host| host.fact_resource_cooldowns.len())
                .sum::<usize>();
            if !already_present && total >= MAX_FACT_RESOURCE_COOLDOWNS {
                evict_earliest_fact_resource_cooldown(hosts);
            }

            let host = result_host_entry(hosts, host_id, Some(source))?;
            if let Some(cursor) = host
                .fact_resource_resume_cursors
                .iter_mut()
                .find(|cursor| cursor.redaction_profile == redaction_profile)
            {
                cursor.candidate = candidate.clone();
            } else {
                host.fact_resource_resume_cursors
                    .push(RemoteFactResourceResumeCursor {
                        redaction_profile,
                        candidate: candidate.clone(),
                    });
            }
            host.fact_resource_resume_cursors
                .sort_by_key(|cursor| cursor.redaction_profile.directory_name());
            if let Some(existing) = host
                .fact_resource_cooldowns
                .iter_mut()
                .find(|cooldown| cooldown.matches(redaction_profile, candidate))
            {
                existing.retry_at = retry_at;
            } else {
                if host.fact_resource_cooldowns.len() >= MAX_FACT_RESOURCE_COOLDOWNS_PER_HOST {
                    let remove = host
                        .fact_resource_cooldowns
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, cooldown)| cooldown.retry_at)
                        .map(|(index, _)| index)
                        .ok_or_else(|| {
                            invalid_data("remote fact resource cooldown bound is inconsistent")
                        })?;
                    host.fact_resource_cooldowns.remove(remove);
                }
                host.fact_resource_cooldowns
                    .push(RemoteFactResourceCooldown {
                        redaction_profile,
                        candidate: candidate.clone(),
                        retry_at,
                    });
            }
            host.fact_resource_cooldowns.sort_by(|left, right| {
                left.redaction_profile
                    .directory_name()
                    .cmp(right.redaction_profile.directory_name())
                    .then_with(|| left.candidate.cmp(&right.candidate))
            });
            Ok(())
        })
    }

    pub fn record_success(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        report: &RemoteSyncReport,
        next_eligible_at: Option<DateTime<Utc>>,
    ) -> io::Result<RemoteSyncHostHealth> {
        self.record_success_inner(
            host_id,
            source,
            completed_at,
            report,
            next_eligible_at,
            None,
        )
    }

    /// Atomically records a valid aggregate commit and a process-containment
    /// pause raised by a secondary exchange in the same host attempt.
    pub fn record_success_with_process_containment(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        completed_at: DateTime<Utc>,
        report: &RemoteSyncReport,
        configured_host: &RemoteHostConfig,
    ) -> io::Result<RemoteSyncHostHealth> {
        if configured_host.id() != host_id || configured_host.expected_source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote process-containment success does not match its configured host pin",
            ));
        }
        self.record_success_inner(
            host_id,
            Some(source),
            completed_at,
            report,
            None,
            Some(configured_host.automatic_sync_fingerprint()),
        )
    }

    fn record_success_inner(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        report: &RemoteSyncReport,
        next_eligible_at: Option<DateTime<Utc>>,
        process_containment_host_fingerprint: Option<String>,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        validate_next_eligible(completed_at, next_eligible_at)?;
        let pages_committed = u32::try_from(report.pages_committed)
            .map_err(|_| invalid_data("remote sync health pagesCommitted overflows u32"))?;
        let changes_committed = u64::try_from(report.changes_committed)
            .map_err(|_| invalid_data("remote sync health changesCommitted overflows u64"))?;
        let response_bytes = u64::try_from(report.response_bytes)
            .map_err(|_| invalid_data("remote sync health responseBytes overflows u64"))?;
        if pages_committed > MAX_PAGES_PER_ATTEMPT {
            return Err(invalid_data(
                "remote sync health pagesCommitted exceeds the supported limit",
            ));
        }
        if response_bytes > MAX_RESPONSE_BYTES_PER_ATTEMPT {
            return Err(invalid_data(
                "remote sync health responseBytes exceeds the supported limit",
            ));
        }
        let completion = RemoteSyncHealthCompletion::from(&report.completion);
        let source = source.cloned();

        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, source.as_ref())?;
            if host
                .last_attempt_at
                .is_some_and(|current| completed_at < current)
            {
                // A concurrent attempt with a later completion timestamp has
                // already won the durable latest-result slot. Preserve every
                // one of its fields, including source generation and failure
                // streak, instead of letting a delayed writer roll health
                // backwards.
                host.configured = true;
                return Ok(host.clone());
            }
            host.configured = true;
            host.last_attempt_at = Some(completed_at);
            host.last_success_at = Some(completed_at);
            if report.has_activity() {
                host.last_activity_at = Some(completed_at);
            }
            host.last_result = Some(RemoteSyncAttemptResult::Success);
            host.completion = Some(completion);
            host.pages_committed = pages_committed;
            host.changes_committed = changes_committed;
            host.response_bytes = response_bytes;
            host.consecutive_failures = 0;
            host.next_eligible_at = next_eligible_at;
            host.error_category = None;
            clear_budget_pause_through(host, completed_at);
            clear_process_containment_pause_through(host, completed_at);
            if let Some(host_fingerprint) = process_containment_host_fingerprint {
                host.process_containment_paused_at = Some(completed_at);
                host.process_containment_host_fingerprint = Some(host_fingerprint);
            }
            Ok(host.clone())
        })
    }

    pub fn record_failure(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        category: RemoteSyncErrorCategory,
        next_eligible_at: Option<DateTime<Utc>>,
    ) -> io::Result<RemoteSyncHostHealth> {
        self.record_failure_inner(
            host_id,
            source,
            completed_at,
            category,
            next_eligible_at,
            None,
        )
    }

    fn record_failure_inner(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        category: RemoteSyncErrorCategory,
        next_eligible_at: Option<DateTime<Utc>>,
        process_containment_host_fingerprint: Option<String>,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        validate_next_eligible(completed_at, next_eligible_at)?;
        if process_containment_host_fingerprint.is_some()
            && category != RemoteSyncErrorCategory::ProcessContainment
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a remote process-containment host fingerprint requires that error category",
            ));
        }
        let source = source.cloned();

        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, source.as_ref())?;
            if host
                .last_attempt_at
                .is_some_and(|current| completed_at < current)
            {
                host.configured = true;
                return Ok(host.clone());
            }
            host.configured = true;
            host.last_attempt_at = Some(completed_at);
            host.last_result = Some(RemoteSyncAttemptResult::Failure);
            host.completion = None;
            host.pages_committed = 0;
            host.changes_committed = 0;
            host.response_bytes = 0;
            host.consecutive_failures = host.consecutive_failures.saturating_add(1).max(1);
            host.next_eligible_at = next_eligible_at;
            host.error_category = Some(category);
            if let Some(host_fingerprint) = process_containment_host_fingerprint {
                host.process_containment_paused_at = Some(completed_at);
                host.process_containment_host_fingerprint = Some(host_fingerprint);
            }
            clear_budget_pause_through(host, completed_at);
            Ok(host.clone())
        })
    }

    /// Records a rolling-bandwidth policy pause without turning it into a
    /// failed remote attempt or altering the transport failure streak.
    ///
    /// When known, `resume_at` is an absolute UTC instant because persisting a
    /// relative delay would become inaccurate across restarts. `None` retains
    /// a fail-closed pause whose clock or ledger-integrity recovery time cannot
    /// be calculated. A later successful or failed attempt clears a pause it
    /// supersedes.
    pub fn record_pause(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        attempted_at: DateTime<Utc>,
        level: RemoteBandwidthBudgetLevel,
        resume_at: Option<DateTime<Utc>>,
    ) -> io::Result<RemoteSyncHostHealth> {
        self.record_pause_inner(host_id, source, attempted_at, level, resume_at, None)
    }

    /// Atomically records both a bandwidth pause and an SSH containment
    /// residual from its fixed-size recovery probe.
    pub fn record_pause_with_process_containment(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        attempted_at: DateTime<Utc>,
        level: RemoteBandwidthBudgetLevel,
        resume_at: Option<DateTime<Utc>>,
        configured_host: &RemoteHostConfig,
    ) -> io::Result<RemoteSyncHostHealth> {
        if configured_host.id() != host_id || configured_host.expected_source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote process-containment budget pause does not match its configured host pin",
            ));
        }
        self.record_pause_inner(
            host_id,
            Some(source),
            attempted_at,
            level,
            resume_at,
            Some(configured_host.automatic_sync_fingerprint()),
        )
    }

    fn record_pause_inner(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        attempted_at: DateTime<Utc>,
        level: RemoteBandwidthBudgetLevel,
        resume_at: Option<DateTime<Utc>>,
        process_containment_host_fingerprint: Option<String>,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        if resume_at.is_some_and(|resume_at| resume_at < attempted_at) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote sync budget resume time cannot precede its pause",
            ));
        }
        let source = source.cloned();

        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, source.as_ref())?;
            if host
                .last_attempt_at
                .is_some_and(|current| attempted_at < current)
                || host
                    .budget_paused_at
                    .is_some_and(|current| attempted_at < current)
            {
                host.configured = true;
                return Ok(host.clone());
            }
            host.configured = true;
            let hard_pause = level == RemoteBandwidthBudgetLevel::Hard;
            let starts_new_pause = host.budget_paused_at.is_none()
                || (hard_pause && host.budget_probe_due_at.is_none());
            if starts_new_pause {
                host.budget_paused_at = Some(attempted_at);
                host.budget_last_probe_at = None;
                host.budget_last_probe_succeeded = None;
            }
            host.budget_resume_at = resume_at;
            if hard_pause {
                if host.budget_probe_due_at.is_none() {
                    host.budget_probe_due_at = Some(next_budget_probe_deadline(attempted_at)?);
                }
            } else {
                host.budget_probe_due_at = None;
                host.budget_last_probe_at = None;
                host.budget_last_probe_succeeded = None;
            }
            if let Some(host_fingerprint) = process_containment_host_fingerprint {
                host.process_containment_paused_at = Some(attempted_at);
                host.process_containment_host_fingerprint = Some(host_fingerprint);
            }
            Ok(host.clone())
        })
    }

    /// Atomically claims one due hard-pause probe and advances its durable
    /// deadline before any SSH process can start. A crash or concurrent worker
    /// therefore delays, rather than duplicates, the next probe. This touches
    /// no latest-attempt, success, error, or failure-streak fields.
    pub fn claim_due_budget_probe(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        now: DateTime<Utc>,
    ) -> io::Result<bool> {
        validate_host_id(host_id)?;
        self.mutate_if_changed(|hosts| {
            let Some(host) = hosts.get_mut(host_id) else {
                return Ok(false);
            };
            if host.source.as_ref() != Some(source) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "remote sync health source pin conflict for host {host_id:?} while claiming a budget probe"
                    ),
                ));
            }
            let Some(due_at) = host.budget_probe_due_at else {
                return Ok(false);
            };
            if now < due_at {
                return Ok(false);
            }
            host.budget_probe_due_at = Some(next_budget_probe_deadline(now)?);
            host.budget_last_probe_at = Some(now);
            host.budget_last_probe_succeeded = None;
            Ok(true)
        })
    }

    /// Stores only a sanitized outcome for the exact claimed probe. A stale
    /// completion (including one from an earlier process) cannot overwrite a
    /// newer claim, and no normal attempt/failure/pause field is changed.
    pub fn record_budget_probe_result(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        probed_at: DateTime<Utc>,
        succeeded: bool,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, Some(source))?;
            if host.budget_last_probe_at != Some(probed_at) {
                return Ok(host.clone());
            }
            host.budget_last_probe_succeeded = Some(succeeded);
            Ok(host.clone())
        })
    }

    /// Convenience that immediately discards the raw error text.
    pub fn record_sync_error(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        error: &RemoteSyncError,
        next_eligible_at: Option<DateTime<Utc>>,
    ) -> io::Result<RemoteSyncHostHealth> {
        self.record_failure(
            host_id,
            source,
            completed_at,
            RemoteSyncErrorCategory::from_sync_error(error),
            next_eligible_at,
        )
    }

    /// Records an error from an exact automatic/manual configuration snapshot.
    /// A transport containment residual becomes a durable no-retry pause bound
    /// to that exact host row; ordinary transport failures retain normal
    /// backoff.
    pub fn record_sync_error_for_config(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        completed_at: DateTime<Utc>,
        error: &RemoteSyncError,
        next_eligible_at: Option<DateTime<Utc>>,
        configured_host: &RemoteHostConfig,
    ) -> io::Result<RemoteSyncHostHealth> {
        let category = RemoteSyncErrorCategory::from_sync_error(error);
        self.record_failure_inner(
            host_id,
            source,
            completed_at,
            category,
            next_eligible_at,
            (category == RemoteSyncErrorCategory::ProcessContainment)
                .then(|| configured_host.automatic_sync_fingerprint()),
        )
    }

    /// Persists a containment pause independently from aggregate success or
    /// bandwidth-pause accounting. This is used when a secondary automatic
    /// exchange fails after a valid aggregate page was already committed.
    pub fn record_process_containment_pause(
        &self,
        host_id: &str,
        source: &SourceGeneration,
        observed_at: DateTime<Utc>,
        configured_host: &RemoteHostConfig,
    ) -> io::Result<RemoteSyncHostHealth> {
        validate_host_id(host_id)?;
        if configured_host.id() != host_id || configured_host.expected_source() != Some(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote process-containment pause does not match its configured host pin",
            ));
        }
        let host_fingerprint = configured_host.automatic_sync_fingerprint();
        self.mutate(|hosts| {
            let host = result_host_entry(hosts, host_id, Some(source))?;
            if host
                .process_containment_paused_at
                .is_some_and(|current| observed_at < current)
            {
                host.configured = true;
                return Ok(host.clone());
            }
            host.configured = true;
            host.process_containment_paused_at = Some(observed_at);
            host.process_containment_host_fingerprint = Some(host_fingerprint);
            Ok(host.clone())
        })
    }

    /// Clears only a containment pause which predates a successful explicit
    /// probe. The existing attempt/result fields are intentionally untouched.
    pub fn clear_process_containment_pause(
        &self,
        host_id: &str,
        source: Option<&SourceGeneration>,
        succeeded_at: DateTime<Utc>,
    ) -> io::Result<bool> {
        validate_host_id(host_id)?;
        self.mutate_if_changed(|hosts| {
            let Some(host) = hosts.get_mut(host_id) else {
                return Ok(false);
            };
            if let Some(expected) = source
                && host
                    .source
                    .as_ref()
                    .is_some_and(|stored| stored != expected)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote process-containment pause source pin changed",
                ));
            }
            let paused = host
                .process_containment_paused_at
                .is_some_and(|paused_at| paused_at <= succeeded_at);
            if paused {
                clear_process_containment_pause_fields(host);
            }
            Ok(paused)
        })
    }

    /// Reconciles already-loaded connection configuration without probing SSH.
    /// Removed hosts remain as detached health records with `configured=false`.
    pub fn reconcile_configured_hosts(
        &self,
        config: &RemotesConfig,
    ) -> io::Result<Vec<RemoteSyncHostHealth>> {
        config.validate()?;
        self.mutate_if_changed(|hosts| {
            for host in hosts.values_mut() {
                host.configured = false;
            }

            for configured in config.hosts() {
                let host = reconcile_host_entry(
                    hosts,
                    configured.id(),
                    configured.expected_source().cloned(),
                );
                host.configured = true;
                if host.process_containment_host_fingerprint.is_some()
                    && !host.process_containment_paused_for(configured)
                {
                    clear_process_containment_pause_fields(host);
                }
            }

            prune_detached_hosts(hosts)?;
            Ok(hosts.values().cloned().collect())
        })
    }

    fn health_directory(&self) -> PathBuf {
        self.state_root.join(HEALTH_DIRECTORY)
    }

    fn health_path(&self) -> PathBuf {
        self.health_directory().join(HEALTH_FILE)
    }

    fn load(&self) -> io::Result<StoredRemoteSyncHealth> {
        if !private_directory_exists_beneath(&self.state_root, &self.health_directory())? {
            return Ok(StoredRemoteSyncHealth::default());
        }
        let directory = self.health_directory();
        let _lock = open_locked_lock_file(&directory, LockMode::Shared)?;
        read_optional_health(&self.health_path())
    }

    fn mutate<R>(
        &self,
        operation: impl FnOnce(&mut BTreeMap<String, RemoteSyncHostHealth>) -> io::Result<R>,
    ) -> io::Result<R> {
        self.mutate_inner(operation, false)
    }

    fn mutate_if_changed<R>(
        &self,
        operation: impl FnOnce(&mut BTreeMap<String, RemoteSyncHostHealth>) -> io::Result<R>,
    ) -> io::Result<R> {
        self.mutate_inner(operation, true)
    }

    fn mutate_inner<R>(
        &self,
        operation: impl FnOnce(&mut BTreeMap<String, RemoteSyncHostHealth>) -> io::Result<R>,
        skip_unchanged: bool,
    ) -> io::Result<R> {
        let directory = self.health_directory();
        create_private_directory_beneath(&self.state_root, &directory)?;
        let _lock = open_locked_lock_file(&directory, LockMode::Exclusive)?;
        let original = read_optional_health(&self.health_path())?;
        let original_hosts = original.hosts.clone();
        let mut hosts = original.into_map();
        let result = operation(&mut hosts)?;
        // Keep room for newly configured hosts by evicting only the oldest
        // detached records. A long sequence of remove/add operations must not
        // permanently wedge future health updates at the file entry limit.
        prune_detached_hosts(&mut hosts)?;
        if hosts.len() > MAX_HEALTH_HOSTS {
            return Err(invalid_data(format!(
                "remote sync health has {} hosts; maximum is {MAX_HEALTH_HOSTS}",
                hosts.len()
            )));
        }
        let state = StoredRemoteSyncHealth::from_map(hosts);
        state.validate()?;
        if !skip_unchanged || state.hosts != original_hosts {
            write_health_atomically(&self.health_path(), &state)?;
        }
        Ok(result)
    }
}

fn result_host_entry<'a>(
    hosts: &'a mut BTreeMap<String, RemoteSyncHostHealth>,
    host_id: &str,
    source: Option<&SourceGeneration>,
) -> io::Result<&'a mut RemoteSyncHostHealth> {
    if let Some(existing_source) = hosts.get(host_id).and_then(RemoteSyncHostHealth::source) {
        match source {
            Some(attempt_source) if existing_source != attempt_source => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "remote sync health source pin conflict for host {host_id:?}: stored {}@{} but attempt used {}@{}",
                        existing_source.node_id,
                        existing_source.generation,
                        attempt_source.node_id,
                        attempt_source.generation,
                    ),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "remote sync health source pin conflict for host {host_id:?}: stored {}@{} but the attempt had no complete source pin",
                        existing_source.node_id, existing_source.generation,
                    ),
                ));
            }
            Some(_) => {}
        }
    }

    let host = hosts
        .entry(host_id.to_owned())
        .or_insert_with(|| RemoteSyncHostHealth::new(host_id.to_owned(), true, source.cloned()));
    if host.source.is_none() {
        host.source = source.cloned();
    }
    Ok(host)
}

/// Configuration is the sole authority allowed to rotate a known health pin.
/// A missing configuration pin remains "unknown" and therefore preserves an
/// already observed source rather than erasing it on unpair.
fn reconcile_host_entry<'a>(
    hosts: &'a mut BTreeMap<String, RemoteSyncHostHealth>,
    host_id: &str,
    configured_source: Option<SourceGeneration>,
) -> &'a mut RemoteSyncHostHealth {
    let rotate = hosts.get(host_id).is_some_and(|existing| {
        configured_source
            .as_ref()
            .is_some_and(|source| existing.source.as_ref() != Some(source))
    });
    if rotate {
        hosts.insert(
            host_id.to_owned(),
            RemoteSyncHostHealth::new(host_id.to_owned(), true, configured_source.clone()),
        );
    }
    hosts
        .entry(host_id.to_owned())
        .or_insert_with(|| RemoteSyncHostHealth::new(host_id.to_owned(), true, configured_source))
}

fn clear_budget_pause_through(host: &mut RemoteSyncHostHealth, completed_at: DateTime<Utc>) {
    if host
        .budget_paused_at
        .is_some_and(|paused_at| paused_at <= completed_at)
    {
        host.budget_paused_at = None;
        host.budget_resume_at = None;
        host.budget_probe_due_at = None;
        host.budget_last_probe_at = None;
        host.budget_last_probe_succeeded = None;
    }
}

fn clear_process_containment_pause_through(
    host: &mut RemoteSyncHostHealth,
    completed_at: DateTime<Utc>,
) {
    if host
        .process_containment_paused_at
        .is_some_and(|paused_at| paused_at <= completed_at)
    {
        clear_process_containment_pause_fields(host);
    }
}

fn clear_process_containment_pause_fields(host: &mut RemoteSyncHostHealth) {
    host.process_containment_paused_at = None;
    host.process_containment_host_fingerprint = None;
}

fn evict_earliest_fact_resource_cooldown(hosts: &mut BTreeMap<String, RemoteSyncHostHealth>) {
    let earliest = hosts
        .iter()
        .flat_map(|(host_id, host)| {
            host.fact_resource_cooldowns
                .iter()
                .enumerate()
                .map(move |(index, cooldown)| (cooldown.retry_at, host_id.clone(), index))
        })
        .min();
    if let Some((_, host_id, index)) = earliest
        && let Some(host) = hosts.get_mut(&host_id)
    {
        host.fact_resource_cooldowns.remove(index);
    }
}

fn next_budget_probe_deadline(from: DateTime<Utc>) -> io::Result<DateTime<Utc>> {
    from.checked_add_signed(TimeDelta::seconds(
        REMOTE_SYNC_HARD_PAUSE_PROBE_INTERVAL_SECONDS,
    ))
    .ok_or_else(|| invalid_data("remote sync budget probe deadline overflows"))
}

fn prune_detached_hosts(hosts: &mut BTreeMap<String, RemoteSyncHostHealth>) -> io::Result<()> {
    let overflow = hosts.len().saturating_sub(MAX_HEALTH_HOSTS);
    if overflow == 0 {
        return Ok(());
    }
    let mut detached = hosts
        .values()
        .filter(|host| !host.configured)
        .map(|host| {
            (
                host.last_attempt_at.max(host.last_fact_sync_at),
                host.host_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    detached.sort();
    if detached.len() < overflow {
        return Err(invalid_data(format!(
            "remote sync health has more than {MAX_HEALTH_HOSTS} configured hosts"
        )));
    }
    for (_, host_id) in detached.into_iter().take(overflow) {
        hosts.remove(&host_id);
    }
    Ok(())
}

fn validate_next_eligible(
    completed_at: DateTime<Utc>,
    next_eligible_at: Option<DateTime<Utc>>,
) -> io::Result<()> {
    if next_eligible_at.is_some_and(|next| next < completed_at) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync next eligible time cannot precede its attempt",
        ));
    }
    Ok(())
}

fn validate_host_id(id: &str) -> io::Result<()> {
    if !host_id_has_valid_shape(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "remote sync host ID must contain 1 to {MAX_HOST_ID_BYTES} bytes and use only the configured host-ID alphabet"
            ),
        ));
    }
    Ok(())
}

fn validate_stored_host_id(id: &str) -> io::Result<()> {
    if host_id_has_valid_shape(id) {
        Ok(())
    } else {
        Err(invalid_data(
            "remote sync health contains an invalid host ID",
        ))
    }
}

fn valid_remote_host_fingerprint(value: &str) -> bool {
    value
        .strip_prefix(REMOTE_HOST_FINGERPRINT_PREFIX)
        .is_some_and(|hex| {
            hex.len() == REMOTE_HOST_FINGERPRINT_HEX_BYTES
                && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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

fn serialize_health(state: &StoredRemoteSyncHealth) -> io::Result<Vec<u8>> {
    state.validate()?;
    let mut contents = serde_json::to_vec_pretty(state)
        .map_err(|error| invalid_data(format!("invalid remote sync health: {error}")))?;
    contents.push(b'\n');
    if contents.len() as u64 > MAX_HEALTH_FILE_BYTES {
        return Err(invalid_data("remote sync health file is too large"));
    }
    Ok(contents)
}

fn deserialize_health(contents: &[u8]) -> io::Result<StoredRemoteSyncHealth> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct VersionProbe {
        schema_version: Option<u32>,
    }

    let version = serde_json::from_slice::<VersionProbe>(contents)
        .map_err(|error| invalid_data(format!("invalid remote sync health: {error}")))?
        .schema_version
        .ok_or_else(|| invalid_data("remote sync health is missing schemaVersion"))?;
    if version != REMOTE_SYNC_HEALTH_SCHEMA_VERSION {
        let relation = if version > REMOTE_SYNC_HEALTH_SCHEMA_VERSION {
            "future"
        } else {
            "unsupported"
        };
        return Err(invalid_data(format!(
            "{relation} remote sync health schema version {version}; expected {REMOTE_SYNC_HEALTH_SCHEMA_VERSION}"
        )));
    }
    let state: StoredRemoteSyncHealth = serde_json::from_slice(contents)
        .map_err(|error| invalid_data(format!("invalid remote sync health: {error}")))?;
    state.validate()?;
    Ok(state)
}

fn read_optional_health(path: &Path) -> io::Result<StoredRemoteSyncHealth> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoredRemoteSyncHealth::default());
        }
        Err(error) => return Err(error),
    }
    let contents = read_private_bounded(path, MAX_HEALTH_FILE_BYTES, "remote sync health file")?;
    deserialize_health(&contents)
}

fn write_health_atomically(path: &Path, state: &StoredRemoteSyncHealth) -> io::Result<()> {
    let contents = serialize_health(state)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync health path has no parent",
        )
    })?;
    validate_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "remote sync health file")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let (temporary_path, mut temporary) = create_temporary_file(parent, HEALTH_FILE)?;
    let result = (|| {
        temporary.write_all(&contents)?;
        temporary.sync_all()?;
        drop(temporary);
        replace_file(&temporary_path, path)?;
        validate_published_private_file(path, "remote sync health file")?;
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

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

fn open_locked_lock_file(directory: &Path, mode: LockMode) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(HEALTH_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_private_file_metadata(&metadata, "remote sync health lock")?,
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
        .map_err(|error| map_nofollow_error(error, "remote sync health lock"))?;
    validate_opened_private_file(&path, &file, "remote sync health lock")?;

    match mode {
        LockMode::Shared => fs2::FileExt::lock_shared(&file)?,
        LockMode::Exclusive => fs2::FileExt::lock_exclusive(&file)?,
    }

    validate_private_directory(directory)?;
    validate_opened_private_file(&path, &file, "remote sync health lock")?;
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
                validate_opened_private_file(&path, &file, "remote sync health temporary file")?;
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a remote sync health temporary file",
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync health state root must be absolute",
        ));
    }
    match validate_state_root(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_state_root(root)?;
        }
        Err(error) => return Err(error),
    }
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync health path is outside its state root",
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote sync health path contains a non-normal component",
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

fn private_directory_exists_beneath(root: &Path, path: &Path) -> io::Result<bool> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync health state root must be absolute",
        ));
    }
    match validate_state_root(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync health path is outside its state root",
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote sync health path contains a non-normal component",
            ));
        };
        current.push(name);
        match validate_private_directory(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
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
            "remote sync health state root must be a real directory",
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
                "remote sync health state root must be owned by the current user",
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("remote sync health state root must have mode 0700 (found {mode:04o})"),
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_private_directory(path, "remote sync health state root")?;
    Ok(())
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_data(
            "remote sync health directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data("remote sync health path must be a directory"));
    }
    ensure_private_path(&metadata, "remote sync health directory")?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "remote sync health directory")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::TimeDelta;
    use serde_json::json;
    use tempfile::tempdir;

    use crate::remote_protocol::SourceGeneration;
    use crate::remote_transport::RemoteTransportError;
    use crate::remotes_config::{RemotesConfigMutation, RemotesConfigStore};
    use crate::source_history::{
        SessionDigestFingerprint, SessionUsageMetrics, SourceSessionDigest,
    };
    use crate::source_model::SessionReplicaKey;

    const NODE_ID: &str = "node-0123456789abcdef0123456789abcdef";
    const LOCAL_NODE_ID: &str = "node-fedcba9876543210fedcba9876543210";

    fn source(node_id: &str, generation: u64) -> SourceGeneration {
        SourceGeneration {
            node_id: node_id.parse().unwrap(),
            generation: NonZeroU64::new(generation).unwrap(),
        }
    }

    fn fact_candidate(
        source_id: NodeId,
        thread_id: &str,
        range_start: DateTime<Utc>,
        fingerprint: char,
    ) -> ReplicaFactCandidateKey {
        let revisions = crate::remote_agent::current_revisions();
        let range_end = range_start + TimeDelta::days(1);
        let fingerprint = format!(
            "session-digest-sha256-v1-{}",
            fingerprint.to_string().repeat(64)
        )
        .parse::<SessionDigestFingerprint>()
        .unwrap();
        let digest = SourceSessionDigest::new(
            SessionReplicaKey::new(source_id.clone(), thread_id.parse().unwrap()),
            range_start,
            range_end,
            range_end,
            fingerprint.clone(),
            fingerprint,
            1,
            true,
            true,
            Vec::new(),
            SessionUsageMetrics {
                metric_revision: revisions.metric.get(),
                estimator_revision: revisions.estimator.get(),
                project_breakdown_revision: revisions.project_breakdown.get(),
                api_pricing_catalog_revision: revisions.api_pricing_catalog.get(),
                ..SessionUsageMetrics::default()
            },
        )
        .unwrap();
        ReplicaFactCandidateKey::from_digest(source_id, thread_id.parse().unwrap(), &digest)
    }

    fn test_store(directory: &tempfile::TempDir) -> RemoteSyncHealthStore {
        RemoteSyncHealthStore::new(directory.path().join("state"))
    }

    fn paired_config(path: &Path, pinned_source: SourceGeneration) -> RemotesConfig {
        let store = RemotesConfigStore::new(path.to_path_buf());
        let config = store.load_or_create().unwrap();
        let config = store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("dev", "example.invalid"),
            )
            .unwrap();
        store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", pinned_source),
            )
            .unwrap()
    }

    fn report(completion: RemoteSyncCompletion) -> RemoteSyncReport {
        RemoteSyncReport {
            pages_committed: 2,
            changes_committed: 17,
            live_state_changed: false,
            response_bytes: 4096,
            completion,
        }
    }

    #[test]
    fn containment_pause_is_host_bound_and_unrelated_config_changes_do_not_unlock_it() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config/remotes.json");
        let pinned_source = source(NODE_ID, 7);
        let config = paired_config(&config_path, pinned_source.clone());
        let host = config.host("dev").unwrap().clone();
        let store = test_store(&directory);
        let at = Utc::now();
        let error = RemoteSyncError::Transport(RemoteTransportError::Cancelled {
            cleanup_error: Some(io::Error::other("escaped helper")),
        });
        let health = store
            .record_sync_error_for_config("dev", Some(&pinned_source), at, &error, None, &host)
            .unwrap();
        assert_eq!(
            health.error_category(),
            Some(RemoteSyncErrorCategory::ProcessContainment)
        );
        assert!(health.process_containment_paused_for(&host));

        let config_store = RemotesConfigStore::new(config_path);
        let unrelated = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::set_intervals(60, 600),
            )
            .unwrap();
        store.reconcile_configured_hosts(&unrelated).unwrap();
        assert!(
            store
                .get("dev")
                .unwrap()
                .unwrap()
                .process_containment_paused_for(unrelated.host("dev").unwrap())
        );

        let edited = config_store
            .update(
                unrelated.config_revision(),
                RemotesConfigMutation::edit_host(
                    "dev",
                    crate::remotes_config::RemoteHostEdit {
                        ssh_host: Some("changed.invalid".to_owned()),
                        agent_executable: None,
                        redact_content: None,
                    },
                ),
            )
            .unwrap();
        store.reconcile_configured_hosts(&edited).unwrap();
        assert!(
            !store
                .get("dev")
                .unwrap()
                .unwrap()
                .process_containment_paused()
        );
    }

    #[test]
    fn aggregate_success_and_secondary_containment_are_committed_atomically() {
        let directory = tempdir().unwrap();
        let pinned_source = source(NODE_ID, 7);
        let config = paired_config(
            &directory.path().join("config/remotes.json"),
            pinned_source.clone(),
        );
        let host = config.host("dev").unwrap();
        let store = test_store(&directory);
        let completed_at = Utc::now();
        let health = store
            .record_success_with_process_containment(
                "dev",
                &pinned_source,
                completed_at,
                &report(RemoteSyncCompletion::Complete),
                host,
            )
            .unwrap();
        assert_eq!(health.last_result(), Some(RemoteSyncAttemptResult::Success));
        assert!(health.process_containment_paused_for(host));

        let reloaded = store.get("dev").unwrap().unwrap();
        assert_eq!(
            reloaded.last_result(),
            Some(RemoteSyncAttemptResult::Success)
        );
        assert!(reloaded.process_containment_paused_for(host));
        assert!(
            store
                .clear_process_containment_pause(
                    "dev",
                    Some(&pinned_source),
                    completed_at + TimeDelta::seconds(1),
                )
                .unwrap()
        );
        assert!(
            !store
                .get("dev")
                .unwrap()
                .unwrap()
                .process_containment_paused()
        );
    }

    #[test]
    fn ordinary_transport_failure_does_not_create_a_containment_pause() {
        let directory = tempdir().unwrap();
        let pinned_source = source(NODE_ID, 7);
        let config = paired_config(
            &directory.path().join("config/remotes.json"),
            pinned_source.clone(),
        );
        let host = config.host("dev").unwrap();
        let store = test_store(&directory);
        let error = RemoteSyncError::Transport(RemoteTransportError::Cancelled {
            cleanup_error: None,
        });
        let health = store
            .record_sync_error_for_config(
                "dev",
                Some(&pinned_source),
                Utc::now(),
                &error,
                None,
                host,
            )
            .unwrap();
        assert_eq!(
            health.error_category(),
            Some(RemoteSyncErrorCategory::Transport)
        );
        assert!(!health.process_containment_paused());
    }

    fn private_write(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn missing_store_is_empty_and_does_not_create_state() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        assert_eq!(store.list().unwrap(), Vec::<RemoteSyncHostHealth>::new());
        assert_eq!(store.get("dev").unwrap(), None);
        assert!(!store.state_root().exists());
    }

    #[test]
    fn fact_attention_is_independent_latest_wins_and_generation_scoped() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let pinned = source(NODE_ID, 1);
        let first = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .record_success(
                "dev",
                Some(&pinned),
                first,
                &report(RemoteSyncCompletion::Complete),
                None,
            )
            .unwrap();
        let attention_at = first + TimeDelta::seconds(1);
        let attention = store
            .record_fact_sync_outcome(
                "dev",
                &pinned,
                attention_at,
                Some(RemoteSyncErrorCategory::ResourceLimit),
            )
            .unwrap();
        assert_eq!(attention.last_success_at(), Some(first));
        assert_eq!(attention.last_attempt_at(), Some(first));
        assert_eq!(attention.last_fact_sync_at(), Some(attention_at));
        assert_eq!(
            attention.fact_sync_error_category(),
            Some(RemoteSyncErrorCategory::ResourceLimit)
        );

        let stale = store
            .record_fact_sync_outcome(
                "dev",
                &pinned,
                first,
                Some(RemoteSyncErrorCategory::Transport),
            )
            .unwrap();
        assert_eq!(stale.last_fact_sync_at(), Some(attention_at));
        assert_eq!(
            stale.fact_sync_error_category(),
            Some(RemoteSyncErrorCategory::ResourceLimit)
        );
        let recovered_at = attention_at + TimeDelta::seconds(1);
        let recovered = store
            .record_fact_sync_outcome("dev", &pinned, recovered_at, None)
            .unwrap();
        assert!(!recovered.fact_sync_needs_attention());

        let rotated = source(NODE_ID, 2);
        let config = paired_config(
            &directory.path().join("config/remotes.json"),
            rotated.clone(),
        );
        store.reconcile_configured_hosts(&config).unwrap();
        let health = store.get("dev").unwrap().unwrap();
        assert_eq!(health.source(), Some(&rotated));
        assert_eq!(health.last_fact_sync_at(), None);
        assert_eq!(health.fact_sync_error_category(), None);
    }

    #[test]
    fn fact_resource_cooldown_is_per_replica_persistent_and_generation_scoped() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let pinned = source(NODE_ID, 1);
        let observed_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let first = fact_candidate(
            pinned.node_id.clone(),
            "thread-first",
            observed_at - TimeDelta::days(1),
            'a',
        );
        let second = fact_candidate(
            pinned.node_id.clone(),
            "thread-second",
            observed_at - TimeDelta::days(2),
            'b',
        );
        let local = fact_candidate(
            LOCAL_NODE_ID.parse().unwrap(),
            "thread-local",
            observed_at - TimeDelta::days(3),
            'c',
        );
        let refreshed_first = fact_candidate(
            pinned.node_id.clone(),
            "thread-first",
            observed_at - TimeDelta::days(1),
            'd',
        );
        store
            .record_fact_resource_cooldown(
                "dev",
                &pinned,
                RedactionProfile::Redacted,
                &first,
                observed_at,
                true,
            )
            .unwrap();
        store
            .record_fact_resource_cooldown(
                "dev",
                &pinned,
                RedactionProfile::Redacted,
                &local,
                observed_at,
                true,
            )
            .unwrap();
        store
            .record_fact_resource_cooldown(
                "dev",
                &pinned,
                RedactionProfile::Redacted,
                &second,
                observed_at,
                false,
            )
            .unwrap();

        assert!(
            store
                .active_fact_resource_exclusions(
                    "dev",
                    &pinned,
                    RedactionProfile::Redacted,
                    observed_at + TimeDelta::minutes(14),
                )
                .unwrap()
                .contains(&second)
        );

        let restarted = RemoteSyncHealthStore::new(store.state_root().to_path_buf());
        let active = restarted
            .active_fact_resource_exclusions(
                "dev",
                &pinned,
                RedactionProfile::Redacted,
                observed_at + TimeDelta::hours(5),
            )
            .unwrap();
        assert!(active.contains(&first));
        assert!(active.contains(&local));
        assert!(!active.contains(&second));
        assert!(!active.contains(&refreshed_first));
        assert!(
            restarted
                .active_fact_resource_exclusions(
                    "dev",
                    &pinned,
                    RedactionProfile::PreviewEnabled,
                    observed_at + TimeDelta::hours(5),
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            restarted
                .active_fact_resource_exclusions(
                    "dev",
                    &pinned,
                    RedactionProfile::Redacted,
                    observed_at + TimeDelta::hours(6),
                )
                .unwrap()
                .is_empty()
        );

        let rotated = source(NODE_ID, 2);
        let config = paired_config(
            &directory.path().join("config/remotes.json"),
            rotated.clone(),
        );
        restarted.reconcile_configured_hosts(&config).unwrap();
        assert!(
            restarted
                .active_fact_resource_exclusions(
                    "dev",
                    &rotated,
                    RedactionProfile::Redacted,
                    observed_at + TimeDelta::hours(1),
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cooldown_overflow_persists_a_fair_resume_cursor_past_thirty_two_failures() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let pinned = source(NODE_ID, 1);
        let observed_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let candidates = (0..33)
            .map(|index| {
                fact_candidate(
                    pinned.node_id.clone(),
                    &format!("thread-{index:02}"),
                    observed_at - TimeDelta::days(1),
                    char::from(b"0123456789abcdef"[index % 16]),
                )
            })
            .collect::<Vec<_>>();
        for candidate in &candidates {
            store
                .record_fact_resource_cooldown(
                    "dev",
                    &pinned,
                    RedactionProfile::Redacted,
                    candidate,
                    observed_at,
                    true,
                )
                .unwrap();
        }

        let restarted = RemoteSyncHealthStore::new(store.state_root().to_path_buf());
        assert_eq!(
            restarted
                .active_fact_resource_exclusions(
                    "dev",
                    &pinned,
                    RedactionProfile::Redacted,
                    observed_at,
                )
                .unwrap()
                .len(),
            MAX_FACT_RESOURCE_COOLDOWNS_PER_HOST
        );
        assert_eq!(
            restarted
                .fact_resource_resume_after("dev", &pinned, RedactionProfile::Redacted)
                .unwrap()
                .as_ref(),
            candidates.last()
        );
        assert!(
            restarted
                .fact_resource_resume_after("dev", &pinned, RedactionProfile::PreviewEnabled)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn success_failures_and_recovery_are_persisted_without_losing_last_success() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let source = source(NODE_ID, 1);
        let first = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let success = store
            .record_success(
                "dev",
                Some(&source),
                first,
                &report(RemoteSyncCompletion::Complete),
                Some(first + TimeDelta::minutes(5)),
            )
            .unwrap();
        assert_eq!(
            success.last_result(),
            Some(RemoteSyncAttemptResult::Success)
        );
        assert_eq!(success.last_success_at(), Some(first));
        assert_eq!(
            success.completion(),
            Some(RemoteSyncHealthCompletion::Complete)
        );
        assert_eq!(success.pages_committed(), 2);
        assert_eq!(success.changes_committed(), 17);
        assert_eq!(success.response_bytes(), 4096);

        let second = first + TimeDelta::minutes(5);
        let failure = store
            .record_failure(
                "dev",
                Some(&source),
                second,
                RemoteSyncErrorCategory::Transport,
                Some(second + TimeDelta::minutes(1)),
            )
            .unwrap();
        assert_eq!(failure.last_success_at(), Some(first));
        assert_eq!(
            failure.last_result(),
            Some(RemoteSyncAttemptResult::Failure)
        );
        assert_eq!(
            failure.error_category(),
            Some(RemoteSyncErrorCategory::Transport)
        );
        assert_eq!(failure.consecutive_failures(), 1);
        assert_eq!(failure.pages_committed(), 0);

        let third = second + TimeDelta::minutes(1);
        assert_eq!(
            store
                .record_failure(
                    "dev",
                    Some(&source),
                    third,
                    RemoteSyncErrorCategory::Remote,
                    Some(third + TimeDelta::minutes(2)),
                )
                .unwrap()
                .consecutive_failures(),
            2
        );

        let recovered_at = third + TimeDelta::minutes(2);
        let recovered = store
            .record_success(
                "dev",
                Some(&source),
                recovered_at,
                &report(RemoteSyncCompletion::Complete),
                Some(recovered_at + TimeDelta::minutes(5)),
            )
            .unwrap();
        assert_eq!(recovered.last_success_at(), Some(recovered_at));
        assert_eq!(recovered.consecutive_failures(), 0);
        assert_eq!(recovered.error_category(), None);
        assert_eq!(store.get("dev").unwrap(), Some(recovered));
    }

    #[test]
    fn delayed_older_completion_cannot_roll_back_newer_health() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let source = source(NODE_ID, 1);
        let older = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let newer = older + TimeDelta::seconds(1);

        let latest = store
            .record_success(
                "dev",
                Some(&source),
                newer,
                &report(RemoteSyncCompletion::Complete),
                Some(newer + TimeDelta::minutes(5)),
            )
            .unwrap();
        let delayed = store
            .record_failure(
                "dev",
                Some(&source),
                older,
                RemoteSyncErrorCategory::Transport,
                Some(older + TimeDelta::minutes(1)),
            )
            .unwrap();

        assert_eq!(delayed, latest);
        assert_eq!(store.get("dev").unwrap(), Some(latest));
    }

    #[test]
    fn reconciliation_rotates_the_complete_same_node_generation_and_resets_health() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let first_source = source(NODE_ID, 1);
        let rotated_source = source(NODE_ID, 2);
        let attempted_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .record_failure(
                "dev",
                Some(&first_source),
                attempted_at,
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();

        let config = paired_config(
            &directory.path().join("rotated/remotes.json"),
            rotated_source.clone(),
        );
        let reconciled = store.reconcile_configured_hosts(&config).unwrap();

        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].source(), Some(&rotated_source));
        assert_eq!(reconciled[0].last_attempt_at(), None);
        assert_eq!(reconciled[0].last_result(), None);
        assert_eq!(reconciled[0].consecutive_failures(), 0);
    }

    #[test]
    fn delayed_old_generation_result_is_rejected_after_config_rotation() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let old_source = source(NODE_ID, 1);
        let new_source = source(NODE_ID, 2);
        let attempted_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .record_success(
                "dev",
                Some(&old_source),
                attempted_at,
                &report(RemoteSyncCompletion::Complete),
                None,
            )
            .unwrap();
        let config = paired_config(
            &directory.path().join("rotated/remotes.json"),
            new_source.clone(),
        );
        store.reconcile_configured_hosts(&config).unwrap();
        let rotated = store.get("dev").unwrap().unwrap();

        let error = store
            .record_failure(
                "dev",
                Some(&old_source),
                attempted_at + TimeDelta::hours(1),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("source pin conflict"));
        assert_eq!(store.get("dev").unwrap().as_ref(), Some(&rotated));

        let unpinned_error = store
            .record_pause(
                "dev",
                None,
                attempted_at + TimeDelta::hours(2),
                RemoteBandwidthBudgetLevel::Hard,
                None,
            )
            .unwrap_err();
        assert_eq!(unpinned_error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            unpinned_error
                .to_string()
                .contains("no complete source pin")
        );
        assert_eq!(store.get("dev").unwrap(), Some(rotated));
    }

    #[test]
    fn configured_new_generation_accepts_its_own_success() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let old_source = source(NODE_ID, 1);
        let new_source = source(NODE_ID, 2);
        let attempted_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .record_failure(
                "dev",
                Some(&old_source),
                attempted_at,
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let config = paired_config(
            &directory.path().join("rotated/remotes.json"),
            new_source.clone(),
        );
        store.reconcile_configured_hosts(&config).unwrap();

        let success = store
            .record_success(
                "dev",
                Some(&new_source),
                attempted_at + TimeDelta::minutes(1),
                &report(RemoteSyncCompletion::Complete),
                None,
            )
            .unwrap();

        assert_eq!(success.source(), Some(&new_source));
        assert_eq!(
            success.last_result(),
            Some(RemoteSyncAttemptResult::Success)
        );
        assert_eq!(success.consecutive_failures(), 0);
        assert!(!success.budget_paused());
    }

    #[test]
    fn budget_pause_is_independent_and_does_not_increment_failures() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let source = source(NODE_ID, 1);
        let failed_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let failure = store
            .record_failure(
                "dev",
                Some(&source),
                failed_at,
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let paused_at = failed_at + TimeDelta::minutes(1);
        let resume_at = paused_at + TimeDelta::hours(23);

        let paused = store
            .record_pause(
                "dev",
                Some(&source),
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                Some(resume_at),
            )
            .unwrap();

        assert!(paused.budget_paused());
        assert_eq!(paused.budget_paused_at(), Some(paused_at));
        assert_eq!(paused.budget_resume_at(), Some(resume_at));
        assert_eq!(
            paused.budget_probe_due_at(),
            Some(paused_at + TimeDelta::minutes(30))
        );
        assert_eq!(paused.last_attempt_at(), failure.last_attempt_at());
        assert_eq!(paused.last_result(), failure.last_result());
        assert_eq!(paused.error_category(), failure.error_category());
        assert_eq!(paused.consecutive_failures(), 1);
        assert_eq!(store.get("dev").unwrap(), Some(paused));

        let unknown = store
            .record_pause(
                "clock",
                None,
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                None,
            )
            .unwrap();
        assert!(unknown.budget_paused());
        assert_eq!(unknown.budget_resume_at(), None);
        assert_eq!(unknown.last_result(), None);
        assert_eq!(unknown.consecutive_failures(), 0);
    }

    #[test]
    fn hard_pause_probe_deadline_survives_restart_and_claims_without_unpausing() {
        let directory = tempdir().unwrap();
        let source = source(NODE_ID, 1);
        let paused_at = DateTime::parse_from_rfc3339("2026-08-31T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let store = test_store(&directory);
        store
            .record_failure(
                "dev",
                Some(&source),
                paused_at,
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let paused = store
            .record_pause(
                "dev",
                Some(&source),
                paused_at,
                RemoteBandwidthBudgetLevel::Hard,
                Some(paused_at + TimeDelta::hours(24)),
            )
            .unwrap();
        assert_eq!(paused.consecutive_failures(), 1);

        let restarted = RemoteSyncHealthStore::new(store.state_root().to_path_buf());
        assert!(
            !restarted
                .claim_due_budget_probe("dev", &source, paused_at + TimeDelta::minutes(29),)
                .unwrap()
        );
        assert!(
            restarted
                .claim_due_budget_probe("dev", &source, paused_at + TimeDelta::minutes(30),)
                .unwrap()
        );
        assert!(
            !restarted
                .claim_due_budget_probe("dev", &source, paused_at + TimeDelta::minutes(59),)
                .unwrap()
        );
        let claimed = restarted.get("dev").unwrap().unwrap();
        assert!(claimed.budget_paused());
        assert_eq!(claimed.consecutive_failures(), 1);
        assert_eq!(
            claimed.last_result(),
            Some(RemoteSyncAttemptResult::Failure)
        );
        assert_eq!(
            claimed.budget_probe_due_at(),
            Some(paused_at + TimeDelta::minutes(60))
        );

        let repeated_pause = restarted
            .record_pause(
                "dev",
                Some(&source),
                paused_at + TimeDelta::minutes(31),
                RemoteBandwidthBudgetLevel::Hard,
                Some(paused_at + TimeDelta::hours(24)),
            )
            .unwrap();
        assert_eq!(
            repeated_pause.budget_probe_due_at(),
            Some(paused_at + TimeDelta::minutes(60))
        );
        assert_eq!(repeated_pause.budget_paused_at(), Some(paused_at));
        assert_eq!(repeated_pause.consecutive_failures(), 1);
    }

    #[test]
    fn concurrent_updates_do_not_lose_independent_hosts() {
        let directory = tempdir().unwrap();
        let store = Arc::new(test_store(&directory));
        let start = Arc::new(Barrier::new(17));
        let attempted_at = Utc::now();
        let mut workers = Vec::new();
        for index in 0..16 {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                start.wait();
                store
                    .record_failure(
                        &format!("host-{index}"),
                        None,
                        attempted_at,
                        RemoteSyncErrorCategory::Transport,
                        Some(attempted_at + TimeDelta::minutes(1)),
                    )
                    .unwrap();
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let hosts = store.list().unwrap();
        assert_eq!(hosts.len(), 16);
        assert!(hosts.iter().all(|host| host.consecutive_failures() == 1));

        let start = Arc::new(Barrier::new(17));
        let mut workers = Vec::new();
        for _ in 0..16 {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                start.wait();
                store
                    .record_failure(
                        "shared",
                        None,
                        attempted_at,
                        RemoteSyncErrorCategory::Transport,
                        Some(attempted_at + TimeDelta::minutes(1)),
                    )
                    .unwrap();
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            store.get("shared").unwrap().unwrap().consecutive_failures(),
            16
        );
    }

    #[test]
    fn future_schema_corruption_unknown_fields_and_duplicates_fail_closed() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let path = store.health_path();

        private_write(&path, br#"{"schemaVersion":4,"hosts":[]}"#);
        let error = store.list().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("future"));

        private_write(&path, br#"{"schemaVersion":1,"hosts":[]}"#);
        let error = store.list().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unsupported"));

        private_write(&path, br#"{"schemaVersion":3,"hosts":[],"extra":true}"#);
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        private_write(&path, b"not-json");
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        private_write(
            &path,
            br#"{"schemaVersion":3,"hosts":[{"hostId":"../bad","configured":false,"pagesCommitted":0,"changesCommitted":0,"responseBytes":0,"consecutiveFailures":0}]}"#,
        );
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        private_write(
            &path,
            br#"{"schemaVersion":3,"hosts":[{"hostId":"dev","configured":true,"pagesCommitted":0,"changesCommitted":0,"responseBytes":0,"consecutiveFailures":0},{"hostId":"dev","configured":false,"pagesCommitted":0,"changesCommitted":0,"responseBytes":0,"consecutiveFailures":0}]}"#,
        );
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn file_size_and_host_count_are_bounded() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let path = store.health_path();
        private_write(&path, &vec![b' '; MAX_HEALTH_FILE_BYTES as usize + 1]);
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        let hosts = (0..=MAX_HEALTH_HOSTS)
            .map(|index| {
                json!({
                    "hostId": format!("h{index:04}"),
                    "configured": false,
                    "pagesCommitted": 0,
                    "changesCommitted": 0,
                    "responseBytes": 0,
                    "consecutiveFailures": 0
                })
            })
            .collect::<Vec<_>>();
        private_write(
            &path,
            &serde_json::to_vec(&json!({
                "schemaVersion": REMOTE_SYNC_HEALTH_SCHEMA_VERSION,
                "hosts": hosts
            }))
            .unwrap(),
        );
        let error = store.list().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("maximum"));
    }

    #[test]
    fn reconciliation_is_offline_and_marks_removed_hosts_detached() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let config_store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let mut config = config_store.load_or_create().unwrap();
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("dev", "secret.example"),
            )
            .unwrap();
        let pinned_source = source(NODE_ID, 1);
        config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::pair_pin("dev", pinned_source.clone()),
            )
            .unwrap();

        let reconciled = store.reconcile_configured_hosts(&config).unwrap();
        assert_eq!(reconciled.len(), 1);
        assert!(reconciled[0].configured());
        assert_eq!(reconciled[0].node_id().unwrap().as_str(), NODE_ID);

        let attempted_at = Utc::now();
        store
            .record_success(
                "dev",
                Some(&pinned_source),
                attempted_at,
                &report(RemoteSyncCompletion::Complete),
                None,
            )
            .unwrap();
        let config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::unpair_host("dev"),
            )
            .unwrap();
        let unpaired = store.reconcile_configured_hosts(&config).unwrap();
        assert!(unpaired[0].configured());
        assert_eq!(unpaired[0].node_id().unwrap().as_str(), NODE_ID);
        assert_eq!(unpaired[0].last_success_at(), Some(attempted_at));

        let config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::remove_host("dev"),
            )
            .unwrap();
        let detached = store.reconcile_configured_hosts(&config).unwrap();
        assert_eq!(detached.len(), 1);
        assert!(!detached[0].configured());
        assert_eq!(detached[0].node_id().unwrap().as_str(), NODE_ID);
    }

    #[test]
    fn raw_errors_ssh_aliases_and_paths_never_enter_serialized_health() {
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        let secret_alias = "secret.example";
        let secret_path = "/Users/alice/.ssh/private-key";
        let config_store = RemotesConfigStore::new(directory.path().join("config/remotes.json"));
        let config = config_store.load_or_create().unwrap();
        let config = config_store
            .update(
                config.config_revision(),
                RemotesConfigMutation::add_host("safe-id", secret_alias),
            )
            .unwrap();
        store.reconcile_configured_hosts(&config).unwrap();

        let raw = RemoteSyncError::Local(io::Error::other(format!(
            "transport leaked {secret_alias} at {secret_path}"
        )));
        store
            .record_sync_error("safe-id", None, Utc::now(), &raw, None)
            .unwrap();
        let serialized = fs::read_to_string(store.health_path()).unwrap();
        assert!(!serialized.contains(secret_alias));
        assert!(!serialized.contains(secret_path));
        assert!(!serialized.contains("transport leaked"));
        assert!(serialized.contains("safe-id"));
        assert!(serialized.contains("local_state"));
    }

    #[test]
    fn windows_stable_lock_policy_excludes_delete_sharing() {
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        let mode = stable_lock_share_mode();
        assert_eq!(mode & (FILE_SHARE_READ | FILE_SHARE_WRITE), 0x3);
        assert_eq!(mode & FILE_SHARE_DELETE, 0);
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_files_and_lock_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let store = test_store(&directory);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        assert_eq!(
            fs::metadata(store.state_root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.health_directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.health_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.health_directory().join(HEALTH_LOCK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn permissive_files_and_symlinked_directories_files_or_locks_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let permissions_case = tempdir().unwrap();
        let store = test_store(&permissions_case);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        fs::set_permissions(store.health_path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.list().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        let directory_case = tempdir().unwrap();
        let store = test_store(&directory_case);
        fs::create_dir(&store.state_root).unwrap();
        fs::set_permissions(&store.state_root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory_case.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, store.health_directory()).unwrap();
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        let file_case = tempdir().unwrap();
        let store = test_store(&file_case);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        fs::remove_file(store.health_path()).unwrap();
        let target = file_case.path().join("target.json");
        private_write(&target, br#"{"schemaVersion":1,"hosts":[]}"#);
        symlink(&target, store.health_path()).unwrap();
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);

        let lock_case = tempdir().unwrap();
        let store = test_store(&lock_case);
        store
            .record_failure(
                "dev",
                None,
                Utc::now(),
                RemoteSyncErrorCategory::Transport,
                None,
            )
            .unwrap();
        let lock = store.health_directory().join(HEALTH_LOCK_FILE);
        fs::remove_file(&lock).unwrap();
        let target = lock_case.path().join("target.lock");
        private_write(&target, b"");
        symlink(&target, lock).unwrap();
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
