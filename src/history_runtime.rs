//! Explicit ownership-aware bridge between legacy local history and the v2
//! source-aware history layout.
//!
//! Construction resolves and validates one stable filesystem namespace, but
//! deliberately does not initialize ownership or migrate data. Callers choose
//! the cutover point by invoking [`HistoryRuntime::ensure_v2_active`].
//! The ownership lease fences upgraded, cooperating writers only. Before that
//! explicit call, orchestration must stop any legacy binary that does not know
//! about the ownership manifest; this module cannot prove such a process is
//! absent.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Utc};

use crate::domain::TaskRecord;
use crate::git_repository::GitProjectEvidenceResolver;
use crate::history::{
    HistoryObservation, HistoryStore, HistoryWriteReport, SummaryBackfillAttempt,
    default_history_root,
};
use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, InitializeV1Outcome,
    OwnershipCasOutcome, OwnershipManifestStatus,
};
use crate::history_query::{
    HistorySourceSelection, UnifiedHistoryBackend, UnifiedHistorySnapshot,
    load_unified_history_since_selected_with_project_mapping_store as query_unified_history_since_selected,
    load_unified_history_since_with_project_mapping_store as query_unified_history_since,
};
use crate::local_history_migration::{
    LocalV1MigrationOptions, activate_local_v2_history, migrate_local_v1_history,
};
use crate::project_mapping::{
    PROJECT_MAPPING_REGISTRATION_FAILED_WARNING, ProjectMappingStore, ProjectObservation,
};
#[cfg(test)]
use crate::service::try_acquire_service_cutover_exclusive_at;
use crate::service::{
    ServiceDefinitionObservation, TryRecorderInstanceLock,
    current_user_service_definition_observation, ensure_no_recorder_cutover_blocker_at,
    ensure_service_definition_is_trusted_at, service_coordination_root,
    try_acquire_service_cutover_shared_at,
};
use crate::source_export::{
    LocalSessionDigestEvidence, finalize_local_session_digests,
    local_project_descriptors_with_resolver, normalize_observation_project_keys,
};
use crate::source_history::{
    HistoryProfileId, LocalObservationGarbageCollectionReport, LocalObservationMode,
    LocalObservationWriteReport, RedactionProfile, SourceHistoryStore, SourceHistoryWriter,
    SourceSessionDigest, V2SummaryBackfillAttempt,
};
use crate::source_identity::{SourceIdentity, SourceIdentityStore};
use crate::source_model::ObservedProjectKey;

const LEGACY_HISTORY_DIRECTORY: &str = "history-v1";
const SOURCE_IDENTITY_FILE: &str = "source-identity.json";
const LOCAL_SOURCE_LABEL: &str = "local";
const V2_GARBAGE_COLLECTION_INTERVAL: StdDuration = StdDuration::from_secs(6 * 60 * 60);
const V2_GARBAGE_COLLECTION_PROCESS_CHECK_INTERVAL: StdDuration = StdDuration::from_secs(5 * 60);
const MAX_GARBAGE_COLLECTION_WARNING_CHARS: usize = 320;
const PROJECT_MAPPING_CAS_ATTEMPTS: usize = 4;

/// Backend-specific persistence details from one runtime-managed flush.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryRuntimeWriteReport {
    V1(HistoryWriteReport),
    V2(LocalObservationWriteReport),
}

#[derive(Clone, Debug)]
struct StagedLocalSessionDigests {
    observed_at: DateTime<Utc>,
    scan_complete: bool,
    digests: Vec<SourceSessionDigest>,
}

/// Stable lower bound for every local-v1 migration.
///
/// A recovery must present exactly the same window as the durable migration
/// marker. Using wall-clock-relative retention here would make a crash
/// unrecoverable on the next run. The legacy store applies its own retention
/// bounds, so an epoch lower bound does not make the import unbounded.
fn migration_window_starts_at() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("the Unix epoch is representable")
}

/// One fully bound local-history runtime.
///
/// All stores use the same canonical state-root spelling. In particular this
/// prevents aliases, drive-letter spelling, and Windows namespace selection
/// from creating separate ownership and data domains.
pub struct HistoryRuntime {
    state_root: PathBuf,
    legacy: HistoryStore,
    source_identity: SourceIdentity,
    source_history: SourceHistoryStore,
    ownership: HistoryOwnershipStore,
    project_mapping_store: ProjectMappingStore,
    git_project_evidence: Mutex<GitProjectEvidenceResolver>,
    profile_id: HistoryProfileId,
    redaction_profile: RedactionProfile,
    staged_pending: bool,
    staged_reconcile_required: bool,
    staged_local_session_digests: Option<StagedLocalSessionDigests>,
    last_gc_schedule_check: Mutex<Option<Instant>>,
    pending_runtime_warning: Mutex<Option<String>>,
    #[cfg(test)]
    service_coordination_root_override: Option<PathBuf>,
    #[cfg(test)]
    cutover_pause_after_blocker_check: Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
    #[cfg(test)]
    service_definition_observation_override: Option<ServiceDefinitionObservation>,
}

impl HistoryRuntime {
    /// Resolves the process-default legacy history root.
    ///
    /// This may create the stable source identity when it is genuinely absent,
    /// but it never creates an ownership manifest or migrates history.
    pub fn discover(codex_home: &Path, redact_content: bool) -> io::Result<Self> {
        let history_root = default_history_root().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no user-level history state directory is available",
            )
        })?;
        Self::new(history_root, codex_home, redact_content)
    }

    /// Binds a runtime to an explicit legacy history root.
    ///
    /// The only accepted custom layout is `<state-root>/history-v1`, matching
    /// the migration format and the source-history sibling layout exactly.
    pub fn new(history_root: PathBuf, codex_home: &Path, redact_content: bool) -> io::Result<Self> {
        #[cfg(test)]
        let project_mapping_store = ProjectMappingStore::new(
            history_root
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("test-config/project-mappings.json"),
        );
        #[cfg(not(test))]
        let project_mapping_store = ProjectMappingStore::discover();
        Self::new_with_project_mapping_store(
            history_root,
            codex_home,
            redact_content,
            project_mapping_store,
        )
    }

    /// Binds an explicit project-mapping store for isolated runtimes and
    /// deterministic tests. Production callers use [`Self::new`] or
    /// [`Self::discover`], both of which use the user-level discovered store.
    pub fn new_with_project_mapping_store(
        history_root: PathBuf,
        codex_home: &Path,
        redact_content: bool,
        project_mapping_store: ProjectMappingStore,
    ) -> io::Result<Self> {
        let requested_history_root = strict_absolute_path(&history_root, "legacy history root")?;
        if requested_history_root
            .file_name()
            .and_then(|name| name.to_str())
            != Some(LEGACY_HISTORY_DIRECTORY)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "legacy history root must end in history-v1",
            ));
        }
        let requested_state_root = requested_history_root
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "legacy history root has no state-root parent",
                )
            })?
            .to_path_buf();
        validate_directory_leaf_if_present(&requested_state_root, "history state root")?;
        validate_directory_leaf_if_present(&requested_history_root, "legacy history root")?;

        let codex_home = canonical_existing_directory(codex_home, "Codex home")?;

        // SourceIdentityStore owns the platform-specific private-state-root
        // creation and validation policy. It is intentionally initialized
        // before ownership, which remains untouched until ensure_v2_active.
        let requested_identity_store =
            SourceIdentityStore::at_path(requested_state_root.join(SOURCE_IDENTITY_FILE));
        let source_identity = requested_identity_store.load_or_create()?;

        let state_root = fs::canonicalize(&requested_state_root)?;
        validate_directory_leaf_if_present(&state_root, "history state root")?;
        let canonical_history_root = state_root.join(LEGACY_HISTORY_DIRECTORY);
        if requested_history_root.exists()
            && fs::canonicalize(&requested_history_root)? != canonical_history_root
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy history root does not resolve to the canonical state-root sibling",
            ));
        }

        // Reload through the canonical spelling so identity and later stores
        // prove that they name exactly the same persistence domain.
        let canonical_identity_store =
            SourceIdentityStore::at_path(state_root.join(SOURCE_IDENTITY_FILE));
        let canonical_identity = canonical_identity_store.load()?;
        if canonical_identity != source_identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source identity changed while binding the history runtime",
            ));
        }

        let legacy = HistoryStore::new_with_redaction(
            canonical_history_root.clone(),
            &codex_home,
            redact_content,
        );
        if legacy.history_root() != Some(canonical_history_root.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy history store did not preserve its canonical root binding",
            ));
        }

        let redaction_profile = if redact_content {
            RedactionProfile::Redacted
        } else {
            RedactionProfile::PreviewEnabled
        };
        let profile_text =
            match redaction_profile {
                RedactionProfile::PreviewEnabled => legacy.namespace(),
                RedactionProfile::Redacted => legacy
                    .namespace()
                    .strip_suffix("-redacted")
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "redacted legacy namespace is missing its redaction suffix",
                        )
                    })?,
            };
        let profile_id = profile_text.parse::<HistoryProfileId>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("legacy history namespace is not a valid v2 profile ID: {error}"),
            )
        })?;
        let expected_namespace = match redaction_profile {
            RedactionProfile::PreviewEnabled => profile_id.as_str().to_owned(),
            RedactionProfile::Redacted => format!("{}-redacted", profile_id.as_str()),
        };
        if legacy.namespace() != expected_namespace {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy history namespace does not match the runtime redaction binding",
            ));
        }

        let source_history = SourceHistoryStore::new(state_root.clone(), profile_id.clone());
        let ownership =
            HistoryOwnershipStore::new(state_root.clone(), profile_id.clone(), redaction_profile);
        if source_history.state_root() != state_root
            || source_history.profile_id() != &profile_id
            || ownership.state_root() != state_root
            || ownership.profile_id() != &profile_id
            || ownership.redaction_profile() != redaction_profile
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history stores do not share one exact runtime binding",
            ));
        }

        Ok(Self {
            #[cfg(test)]
            service_coordination_root_override: Some(
                state_root.join("test-current-user-service-registration-scope"),
            ),
            #[cfg(test)]
            cutover_pause_after_blocker_check: None,
            #[cfg(test)]
            service_definition_observation_override: Some(ServiceDefinitionObservation::Absent),
            state_root,
            legacy,
            source_identity: canonical_identity,
            source_history,
            ownership,
            project_mapping_store,
            git_project_evidence: Mutex::new(GitProjectEvidenceResolver::default()),
            profile_id,
            redaction_profile,
            staged_pending: false,
            staged_reconcile_required: false,
            staged_local_session_digests: None,
            last_gc_schedule_check: Mutex::new(None),
            pending_runtime_warning: Mutex::new(None),
        })
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn profile_id(&self) -> &HistoryProfileId {
        &self.profile_id
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    pub fn legacy_history(&self) -> &HistoryStore {
        &self.legacy
    }

    pub fn source_identity(&self) -> &SourceIdentity {
        &self.source_identity
    }

    pub fn source_history(&self) -> &SourceHistoryStore {
        &self.source_history
    }

    pub(crate) fn project_mapping_store(&self) -> &ProjectMappingStore {
        &self.project_mapping_store
    }

    /// Replaces enumerable, machine-local project hashes with the opaque
    /// source-scoped keys used by v2 history and remote descriptors.
    ///
    /// Collection keeps canonical paths only in its task rows; history groups
    /// deliberately retain no path. Normalize while both are still in memory,
    /// before staging or persistence can discard that evidence.
    pub fn normalize_local_collection_observation(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
    ) -> HistoryObservation {
        let mut normalized = observation.clone();
        normalize_observation_project_keys(&self.source_identity, tasks, &mut normalized);
        normalized
    }

    /// Normalizes project IDs and attempts one batch mapping registration.
    ///
    /// The returned observation is always usable, even when the second tuple
    /// item is an error. Callers that persist history must mark that normalized
    /// observation partial instead of dropping usage; use
    /// [`Self::prepare_local_collection_observation`] for that production
    /// policy.
    pub fn normalize_and_register_local_collection_observation(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
    ) -> (HistoryObservation, io::Result<()>) {
        let normalized = self.normalize_local_collection_observation(observation, tasks);
        let registration = self.register_local_project_observations(&normalized, tasks);
        (normalized, registration)
    }

    /// Production-safe normalization: mapping failure never blocks usage
    /// persistence, but every affected bucket receives a durable partial code
    /// which history queries promote to a top-level warning.
    pub fn prepare_local_collection_observation(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
    ) -> HistoryObservation {
        let (mut normalized, registration) =
            self.normalize_and_register_local_collection_observation(observation, tasks);
        if registration.is_err() {
            for bucket in &mut normalized.half_hour_buckets {
                if bucket.project_groups.iter().any(|group| {
                    group
                        .project_id
                        .as_deref()
                        .and_then(|value| value.parse::<ObservedProjectKey>().ok())
                        .is_some()
                }) {
                    bucket
                        .partial_reasons
                        .push(PROJECT_MAPPING_REGISTRATION_FAILED_WARNING.to_owned());
                    bucket.partial_reasons.sort();
                    bucket.partial_reasons.dedup();
                }
            }
        }
        normalized
    }

    fn register_local_project_observations(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
    ) -> io::Result<()> {
        let mut git = self
            .git_project_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        git.begin_collection();
        let descriptors = local_project_descriptors_with_resolver(tasks, observation, &mut git);
        if descriptors.is_empty() {
            return Ok(());
        }
        let observations = descriptors
            .into_iter()
            .map(|descriptor| {
                ProjectObservation::from_remote_descriptor(
                    self.source_identity.node_id().clone(),
                    &descriptor,
                )
            })
            .collect::<io::Result<Vec<_>>>()?;

        for _ in 0..PROJECT_MAPPING_CAS_ATTEMPTS {
            let mappings = self.project_mapping_store.load_or_create()?;
            match self
                .project_mapping_store
                .resolve_or_create_batch(mappings.revision(), observations.clone())
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "project mapping changed repeatedly while registering a local collection; retry later",
        ))
    }

    pub fn ownership(&self) -> &HistoryOwnershipStore {
        &self.ownership
    }

    /// Publishes the initial v1 ownership manifest without starting cutover.
    ///
    /// This is the safe startup path while a legacy recorder may still be
    /// running: uninitialized ownership becomes `V1Active`, while every
    /// already-initialized phase is returned byte-for-byte unchanged. Only an
    /// explicit [`Self::ensure_v2_active`] call may begin migration.
    pub fn ensure_ownership_initialized(&self) -> io::Result<HistoryOwnershipManifest> {
        let lease = self.ownership.acquire_writer_lease()?;
        let manifest = match self.ownership.load_manifest()? {
            OwnershipManifestStatus::Uninitialized => {
                match self.ownership.initialize_v1_active(&lease)? {
                    InitializeV1Outcome::Initialized(manifest)
                    | InitializeV1Outcome::Existing(manifest) => manifest,
                }
            }
            OwnershipManifestStatus::Initialized(manifest) => manifest,
        };
        self.validate_manifest_binding(&manifest)?;
        self.ownership.validate_writer_lease(&lease)?;
        if self.ownership.load_manifest()? != OwnershipManifestStatus::Initialized(manifest.clone())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history ownership changed while initialization was fenced",
            ));
        }
        Ok(manifest)
    }

    /// Loads one ownership-consistent history projection without exposing the
    /// mutable legacy store required by its internal read cache.
    ///
    /// Ownership must already be initialized. `V1Active` and `Migrating`
    /// select legacy history; `V2Active` selects the source-aware aggregate.
    pub fn load_unified_history_since(
        &mut self,
        since: DateTime<Utc>,
    ) -> io::Result<UnifiedHistorySnapshot> {
        let mut snapshot = query_unified_history_since(
            &self.ownership,
            &mut self.legacy,
            &self.source_history,
            &self.project_mapping_store,
            since,
        )?;
        self.append_pending_runtime_warning(&mut snapshot);
        Ok(snapshot)
    }

    /// Loads one exact physical-source projection without falling back to an
    /// all-source aggregate when that source is unavailable.
    pub fn load_unified_history_since_selected(
        &mut self,
        selection: &HistorySourceSelection,
        since: DateTime<Utc>,
    ) -> io::Result<UnifiedHistorySnapshot> {
        let mut snapshot = query_unified_history_since_selected(
            &self.ownership,
            &mut self.legacy,
            &self.source_history,
            &self.project_mapping_store,
            self.source_identity.node_id(),
            selection,
            since,
        )?;
        self.append_pending_runtime_warning(&mut snapshot);
        Ok(snapshot)
    }

    fn append_pending_runtime_warning(&self, snapshot: &mut UnifiedHistorySnapshot) {
        if let Some(warning) = self
            .pending_runtime_warning
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            snapshot.history.warnings.push(warning);
        }
    }

    /// Adds one live observation to the in-memory batching buffer.
    pub fn stage(&mut self, observation: &HistoryObservation) {
        self.legacy.stage(observation);
        self.staged_pending = true;
        // A plain observation has no source-local digest sidecar. Never attach
        // evidence left by an earlier collection to a different observation.
        self.staged_local_session_digests = None;
    }

    /// Adds a complete lookback observation to the in-memory batching buffer.
    ///
    /// V2 callers must later use [`Self::flush_staged_reconcile`] so missing
    /// facts inside the declared window can be tombstoned deliberately.
    pub fn stage_full_observation(&mut self, observation: &HistoryObservation) {
        self.legacy.stage_full_observation(observation);
        self.staged_pending = true;
        self.staged_reconcile_required = true;
        self.staged_local_session_digests = None;
    }

    /// Normalizes and stages one production collection together with its
    /// bounded, content-free session digest sidecar.
    pub fn stage_local_collection(
        &mut self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
        evidence: &LocalSessionDigestEvidence,
    ) -> io::Result<()> {
        let normalized = self.prepare_local_collection_observation(observation, tasks);
        let digests = finalize_local_session_digests(&self.source_identity, evidence, &normalized)?;
        self.legacy.stage(&normalized);
        self.staged_pending = true;
        self.staged_local_session_digests = Some(StagedLocalSessionDigests {
            observed_at: evidence.observed_at(),
            scan_complete: evidence.scan_complete(),
            digests,
        });
        Ok(())
    }

    /// Complete-lookback counterpart to [`Self::stage_local_collection`].
    /// Reconciliation still requires both an explicit reconcile flush and a
    /// sidecar whose rollout scan is proven complete.
    pub fn stage_full_local_collection(
        &mut self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
        evidence: &LocalSessionDigestEvidence,
    ) -> io::Result<()> {
        let normalized = self.prepare_local_collection_observation(observation, tasks);
        let digests = finalize_local_session_digests(&self.source_identity, evidence, &normalized)?;
        self.legacy.stage_full_observation(&normalized);
        self.staged_pending = true;
        self.staged_reconcile_required = true;
        self.staged_local_session_digests = Some(StagedLocalSessionDigests {
            observed_at: evidence.observed_at(),
            scan_complete: evidence.scan_complete(),
            digests,
        });
        Ok(())
    }

    /// Loads durable history and overlays the current in-memory observation.
    pub fn load_unified_history_since_with_staged(
        &mut self,
        since: DateTime<Utc>,
    ) -> io::Result<UnifiedHistorySnapshot> {
        self.load_unified_history_since_with_staged_selected(
            &HistorySourceSelection::AllIncluded,
            since,
        )
    }

    /// Loads a selected durable projection and overlays staged local v1 data
    /// only when the request is all-source or the runtime's exact local ID.
    pub fn load_unified_history_since_with_staged_selected(
        &mut self,
        selection: &HistorySourceSelection,
        since: DateTime<Utc>,
    ) -> io::Result<UnifiedHistorySnapshot> {
        let mut snapshot = self.load_unified_history_since_selected(selection, since)?;
        // A v2 snapshot is already an additive local+remote aggregate. The
        // legacy overlay helper performs key replacement, so applying it here
        // could replace the whole aggregate bucket with only the staged local
        // slice. Until query owns a source-slice overlay, expose durable v2
        // rather than losing or double-counting remote usage.
        let permits_v1_local_overlay = match selection {
            HistorySourceSelection::AllIncluded => true,
            HistorySourceSelection::Local(source_id) => source_id == self.source_identity.node_id(),
            HistorySourceSelection::Remote(_) => false,
        };
        if snapshot.backend == UnifiedHistoryBackend::V1
            && snapshot.source_selection_status.is_applied()
            && permits_v1_local_overlay
        {
            self.legacy
                .overlay_staged_since(&mut snapshot.history, since);
        }
        Ok(snapshot)
    }

    /// Flushes staged live data immediately through the active backend.
    pub fn flush_staged(&mut self) -> io::Result<Option<HistoryRuntimeWriteReport>> {
        self.flush_staged_internal(None, None)
    }

    /// Flushes staged live data when the batching interval has elapsed.
    pub fn flush_staged_if_due(
        &mut self,
        interval: StdDuration,
    ) -> io::Result<Option<HistoryRuntimeWriteReport>> {
        self.flush_staged_internal(Some(interval), None)
    }

    /// Flushes a staged complete lookback using explicit reconciliation.
    ///
    /// V1 preserves its established full-merge behavior. V2 emits tombstones
    /// for previously persisted local facts missing from `[from, to)`.
    pub fn flush_staged_reconcile(
        &mut self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> io::Result<Option<HistoryRuntimeWriteReport>> {
        if from >= to {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staged reconciliation window must be non-empty",
            ));
        }
        self.flush_staged_internal(None, Some((from, to)))
    }

    /// Persists the Summary backfill marker through whichever backend owns
    /// runtime writes. A pending staged observation always downgrades a
    /// requested complete marker to partial.
    pub fn mark_summary_backfill_attempt(
        &mut self,
        completed_at: DateTime<Utc>,
        complete: bool,
    ) -> io::Result<SummaryBackfillAttempt> {
        let lease = self.ownership.acquire_writer_lease()?;
        let manifest = self.load_runtime_write_manifest()?;
        let complete = complete && !self.staged_pending;
        let marker = match manifest.state() {
            HistoryOwnershipState::V1Active => {
                let authority = self.ownership.authorize_v1_write(&lease, &manifest)?;
                self.legacy
                    .writer(&authority)?
                    .mark_summary_backfill_attempt(completed_at, complete)?
            }
            HistoryOwnershipState::V2Active => {
                let authority = self.ownership.authorize_v2_write(&lease, &manifest)?;
                let writer = self.source_history.writer(&authority)?;
                let marker = writer.mark_v2_summary_backfill_attempt(completed_at, complete)?;
                writer.validate()?;
                SummaryBackfillAttempt {
                    completed_at: marker.completed_at,
                    complete: marker.complete,
                }
            }
            HistoryOwnershipState::Migrating => unreachable!("write manifest rejects migration"),
        };
        self.validate_exact_write_manifest(&manifest)?;
        Ok(marker)
    }

    fn flush_staged_internal(
        &mut self,
        due_interval: Option<StdDuration>,
        reconcile: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> io::Result<Option<HistoryRuntimeWriteReport>> {
        let lease = self.ownership.acquire_writer_lease()?;
        let manifest = self.load_runtime_write_manifest()?;

        match manifest.state() {
            HistoryOwnershipState::V1Active => {
                if reconcile.is_some() {
                    let Some(flush) = self.legacy.prepare_staged_flush() else {
                        self.staged_pending = false;
                        self.staged_reconcile_required = false;
                        self.staged_local_session_digests = None;
                        self.validate_exact_write_manifest(&manifest)?;
                        return Ok(None);
                    };
                    if !self.staged_reconcile_required {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "staged reconciliation requires a full observation",
                        ));
                    }
                    debug_assert!(flush.force_full_merge());
                }

                let authority = self.ownership.authorize_v1_write(&lease, &manifest)?;
                let mut writer = self.legacy.writer(&authority)?;
                let result = match (due_interval, reconcile) {
                    (Some(interval), None) => writer.flush_staged_if_due(interval)?,
                    (None, _) => writer.flush_staged()?,
                    (Some(_), Some(_)) => unreachable!("reconciliation is never interval-gated"),
                };
                writer.validate()?;
                self.validate_exact_write_manifest(&manifest)?;
                match result {
                    Some(report) => {
                        if v1_write_succeeded(&report) {
                            self.staged_pending = false;
                            self.staged_reconcile_required = false;
                            self.staged_local_session_digests = None;
                        }
                        Ok(Some(HistoryRuntimeWriteReport::V1(report)))
                    }
                    None => {
                        if due_interval.is_none() {
                            self.staged_pending = false;
                            self.staged_reconcile_required = false;
                            self.staged_local_session_digests = None;
                        }
                        Ok(None)
                    }
                }
            }
            HistoryOwnershipState::V2Active => {
                let flush = match due_interval {
                    Some(interval) => self.legacy.prepare_staged_flush_if_due(interval),
                    None => self.legacy.prepare_staged_flush(),
                };
                let Some(flush) = flush else {
                    self.validate_exact_write_manifest(&manifest)?;
                    if due_interval.is_none() {
                        self.staged_pending = false;
                        self.staged_reconcile_required = false;
                        self.staged_local_session_digests = None;
                    }
                    return Ok(None);
                };
                let mode = match reconcile {
                    Some((from, to)) => {
                        if !self.staged_reconcile_required {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "staged reconciliation requires a full observation",
                            ));
                        }
                        debug_assert!(flush.force_full_merge());
                        LocalObservationMode::Reconcile { from, to }
                    }
                    None => {
                        if self.staged_reconcile_required {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "a staged full observation requires explicit reconciliation",
                            ));
                        }
                        LocalObservationMode::Incremental
                    }
                };

                let authority = self.ownership.authorize_v2_write(&lease, &manifest)?;
                let writer = self.source_history.writer(&authority)?;
                let staged_digests = self.staged_local_session_digests.as_ref();
                if staged_digests
                    .is_some_and(|evidence| evidence.observed_at != flush.observation().observed_at)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "staged local session digests do not match the flushed observation",
                    ));
                }
                let mut report = match staged_digests {
                    Some(evidence) => writer.record_local_observation_with_session_digests(
                        &self.source_identity,
                        LOCAL_SOURCE_LABEL,
                        self.redaction_profile,
                        flush.observation(),
                        mode,
                        &evidence.digests,
                        evidence.scan_complete,
                    )?,
                    None => writer.record_local_observation(
                        &self.source_identity,
                        LOCAL_SOURCE_LABEL,
                        self.redaction_profile,
                        flush.observation(),
                        mode,
                    )?,
                };
                self.maybe_garbage_collect_after_local_write(
                    &writer,
                    flush.observation().observed_at,
                    &mut report,
                );
                writer.validate()?;
                self.validate_exact_write_manifest(&manifest)?;
                let cleared = self.legacy.complete_staged_flush(&flush);
                self.staged_pending = !cleared;
                if cleared {
                    self.staged_reconcile_required = false;
                    self.staged_local_session_digests = None;
                }
                Ok(Some(HistoryRuntimeWriteReport::V2(report)))
            }
            HistoryOwnershipState::Migrating => unreachable!("write manifest rejects migration"),
        }
    }

    /// Persists one local observation in the source-aware namespace.
    ///
    /// Runtime writes never initialize ownership or trigger migration. The
    /// caller must perform the explicit cutover first. A fresh, short-lived
    /// writer lease and an authority for the exact durable v2 epoch fence each
    /// call independently.
    pub fn record_local_observation(
        &self,
        observation: &HistoryObservation,
        mode: LocalObservationMode,
    ) -> io::Result<LocalObservationWriteReport> {
        let lease = self.ownership.acquire_writer_lease()?;
        let active = self.load_exact_v2_active_manifest()?;
        let authority = self.ownership.authorize_v2_write(&lease, &active)?;
        let writer = self.source_history.writer(&authority)?;
        let mut report = writer.record_local_observation(
            &self.source_identity,
            LOCAL_SOURCE_LABEL,
            self.redaction_profile,
            observation,
            mode,
        )?;
        self.maybe_garbage_collect_after_local_write(&writer, observation.observed_at, &mut report);
        writer.validate()?;
        self.validate_exact_active_manifest(&active)?;
        Ok(report)
    }

    /// Production collection write which normalizes project identity before
    /// entering the durable v2 writer. The lower-level method remains useful
    /// for migration and storage tests which already carry canonical IDs.
    pub fn record_local_collection_observation(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
        mode: LocalObservationMode,
    ) -> io::Result<LocalObservationWriteReport> {
        let normalized = self.prepare_local_collection_observation(observation, tasks);
        self.record_local_observation(&normalized, mode)
    }

    /// Production collection write with source-scoped local replica evidence.
    /// The full rollout dataset has already been collapsed into `evidence`, so
    /// this path never retains calls, interactions, messages, or paths.
    pub fn record_local_collection_with_session_digests(
        &self,
        observation: &HistoryObservation,
        tasks: &[TaskRecord],
        evidence: &LocalSessionDigestEvidence,
        mode: LocalObservationMode,
    ) -> io::Result<LocalObservationWriteReport> {
        let normalized = self.prepare_local_collection_observation(observation, tasks);
        let digests = finalize_local_session_digests(&self.source_identity, evidence, &normalized)?;
        let lease = self.ownership.acquire_writer_lease()?;
        let active = self.load_exact_v2_active_manifest()?;
        let authority = self.ownership.authorize_v2_write(&lease, &active)?;
        let writer = self.source_history.writer(&authority)?;
        let mut report = writer.record_local_observation_with_session_digests(
            &self.source_identity,
            LOCAL_SOURCE_LABEL,
            self.redaction_profile,
            &normalized,
            mode,
            &digests,
            evidence.scan_complete(),
        )?;
        self.maybe_garbage_collect_after_local_write(&writer, observation.observed_at, &mut report);
        writer.validate()?;
        self.validate_exact_active_manifest(&active)?;
        Ok(report)
    }

    fn maybe_garbage_collect_after_local_write(
        &self,
        writer: &SourceHistoryWriter<'_, '_, '_>,
        observed_at: DateTime<Utc>,
        report: &mut LocalObservationWriteReport,
    ) {
        let now = Instant::now();
        let should_check = {
            let mut last_check = self
                .last_gc_schedule_check
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last_check.is_some_and(|last| {
                now.saturating_duration_since(last) < V2_GARBAGE_COLLECTION_PROCESS_CHECK_INTERVAL
            }) {
                false
            } else {
                *last_check = Some(now);
                true
            }
        };
        if !should_check {
            return;
        }

        let started = Instant::now();
        match writer.garbage_collect_if_due(observed_at, V2_GARBAGE_COLLECTION_INTERVAL) {
            Ok(None) => {}
            Ok(Some(gc)) => {
                report.garbage_collection = LocalObservationGarbageCollectionReport {
                    attempted: true,
                    duration_us: saturating_duration_us(started.elapsed()),
                    shards_pruned: gc.shards_pruned,
                    pruning_deferred: gc.pruning_deferred,
                    trusted_at: gc.trusted_at,
                    warning: None,
                };
            }
            Err(error) => {
                let warning = bounded_garbage_collection_warning(&error);
                report.garbage_collection = LocalObservationGarbageCollectionReport {
                    attempted: true,
                    duration_us: saturating_duration_us(started.elapsed()),
                    warning: Some(warning.clone()),
                    ..LocalObservationGarbageCollectionReport::default()
                };
                *self
                    .pending_runtime_warning
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(warning);
            }
        }
    }

    /// Monotonically records the v2 Summary reconstruction status for the
    /// current ownership epoch. This is intentionally unavailable before the
    /// explicit v2 cutover completes.
    pub fn mark_v2_summary_backfill_attempt(
        &self,
        completed_at: DateTime<Utc>,
        complete: bool,
    ) -> io::Result<V2SummaryBackfillAttempt> {
        let lease = self.ownership.acquire_writer_lease()?;
        let active = self.load_exact_v2_active_manifest()?;
        let authority = self.ownership.authorize_v2_write(&lease, &active)?;
        let writer = self.source_history.writer(&authority)?;
        let marker = writer
            .mark_v2_summary_backfill_attempt(completed_at, complete && !self.staged_pending)?;
        writer.validate()?;
        self.validate_exact_active_manifest(&active)?;
        Ok(marker)
    }

    /// Loads the v2 Summary reconstruction status without changing it.
    ///
    /// The current storage API deliberately places marker reads on the fenced
    /// writer surface, so this read also takes a short lease and pins the exact
    /// active ownership epoch. It does not create the marker.
    pub fn load_v2_summary_backfill_attempt(&self) -> io::Result<Option<V2SummaryBackfillAttempt>> {
        let lease = self.ownership.acquire_writer_lease()?;
        let active = self.load_exact_v2_active_manifest()?;
        let authority = self.ownership.authorize_v2_write(&lease, &active)?;
        let writer = self.source_history.writer(&authority)?;
        let marker = writer.load_v2_summary_backfill_attempt()?;
        writer.validate()?;
        self.validate_exact_active_manifest(&active)?;
        Ok(marker)
    }

    /// Explicitly migrates this namespace to v2, or returns the already-active
    /// v2 manifest without changing it.
    ///
    /// Initialization, import, crash recovery, verification, and activation
    /// all occur while one ownership writer lease is held. The caller must
    /// first quiesce legacy binaries that do not participate in that lease.
    pub fn ensure_v2_active(&mut self) -> io::Result<HistoryOwnershipManifest> {
        self.ensure_v2_active_at(Utc::now())
    }

    fn ensure_v2_active_at(
        &mut self,
        completed_at: DateTime<Utc>,
    ) -> io::Result<HistoryOwnershipManifest> {
        let window_starts_at = migration_window_starts_at();
        let completed_at = completed_at.max(window_starts_at);
        let lease = self.ownership.acquire_writer_lease()?;
        let manifest_status = self.ownership.load_manifest()?;
        if let OwnershipManifestStatus::Initialized(manifest) = &manifest_status {
            self.validate_manifest_binding(manifest)?;
            if manifest.state() == HistoryOwnershipState::V2Active {
                return Ok(manifest.clone());
            }
        }
        let coordination_root = self.service_coordination_root_for_cutover()?;
        let _service_gate = match try_acquire_service_cutover_shared_at(&coordination_root)? {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "history cutover is deferred while the current-user service registration is changing",
                ));
            }
        };
        // A failed service replacement can leave an old automatic-start
        // registration whose future execution cannot be excluded. The
        // durable state-root marker deliberately blocks both a fresh cutover
        // and recovery from Migrating; it has no stale timeout.
        ensure_no_recorder_cutover_blocker_at(&coordination_root)?;
        ensure_service_definition_is_trusted_at(
            &coordination_root,
            self.service_definition_observation_for_cutover()?,
        )?;
        #[cfg(test)]
        if let Some((entered, resume)) = self.cutover_pause_after_blocker_check.as_ref() {
            entered.wait();
            resume.wait();
        }

        let manifest = match manifest_status {
            OwnershipManifestStatus::Uninitialized => {
                match self.ownership.initialize_v1_active(&lease)? {
                    InitializeV1Outcome::Initialized(manifest)
                    | InitializeV1Outcome::Existing(manifest) => manifest,
                }
            }
            OwnershipManifestStatus::Initialized(manifest) => manifest,
        };
        self.validate_manifest_binding(&manifest)?;

        let migrating = match manifest.state() {
            HistoryOwnershipState::V2Active => return Ok(manifest),
            HistoryOwnershipState::Migrating => manifest,
            HistoryOwnershipState::V1Active => {
                match self.ownership.begin_migration(&lease, &manifest)? {
                    OwnershipCasOutcome::Applied(manifest) => manifest,
                    OwnershipCasOutcome::Conflict(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "history ownership changed while the writer lease was held",
                        ));
                    }
                }
            }
        };
        self.validate_manifest_binding(&migrating)?;
        if migrating.state() != HistoryOwnershipState::Migrating {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history cutover did not enter the migrating state",
            ));
        }

        let options = LocalV1MigrationOptions {
            source_identity: &self.source_identity,
            redaction_profile: self.redaction_profile,
            source_label: LOCAL_SOURCE_LABEL,
            expected_ownership_epoch: migrating.epoch(),
            window_starts_at,
            completed_at,
        };
        migrate_local_v1_history(
            &mut self.legacy,
            &self.source_history,
            &self.ownership,
            &lease,
            &migrating,
            &options,
        )?;
        let activation = activate_local_v2_history(
            &mut self.legacy,
            &self.source_history,
            &self.ownership,
            &lease,
            &migrating,
            &options,
        )?;
        let active = activation.ownership().clone();
        self.validate_manifest_binding(&active)?;
        if active.state() != HistoryOwnershipState::V2Active
            || self.ownership.load_manifest()?
                != OwnershipManifestStatus::Initialized(active.clone())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "v2 activation was not durably published in this runtime namespace",
            ));
        }
        self.ownership.validate_writer_lease(&lease)?;
        Ok(active)
    }

    fn service_coordination_root_for_cutover(&self) -> io::Result<PathBuf> {
        #[cfg(test)]
        if let Some(root) = self.service_coordination_root_override.as_ref() {
            return Ok(root.clone());
        }
        service_coordination_root()
    }

    fn service_definition_observation_for_cutover(
        &self,
    ) -> io::Result<ServiceDefinitionObservation> {
        #[cfg(test)]
        if let Some(observation) = self.service_definition_observation_override.as_ref() {
            return Ok(observation.clone());
        }
        current_user_service_definition_observation()
            .map_err(|error| io::Error::other(format!("{error:#}")))
    }

    #[cfg(test)]
    pub(crate) fn set_service_coordination_root_for_test(&mut self, root: PathBuf) {
        self.service_coordination_root_override = Some(root);
    }

    #[cfg(test)]
    fn pause_cutover_after_blocker_check_for_test(
        &mut self,
        entered: std::sync::Arc<std::sync::Barrier>,
        resume: std::sync::Arc<std::sync::Barrier>,
    ) {
        self.cutover_pause_after_blocker_check = Some((entered, resume));
    }

    fn validate_manifest_binding(&self, manifest: &HistoryOwnershipManifest) -> io::Result<()> {
        if manifest.profile_id() != &self.profile_id
            || manifest.redaction_profile() != self.redaction_profile
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history ownership manifest does not match the runtime namespace",
            ));
        }
        Ok(())
    }

    fn load_runtime_write_manifest(&self) -> io::Result<HistoryOwnershipManifest> {
        let manifest = match self.ownership.load_manifest()? {
            OwnershipManifestStatus::Uninitialized => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "history ownership is uninitialized; initialize it before writing",
                ));
            }
            OwnershipManifestStatus::Initialized(manifest) => manifest,
        };
        self.validate_manifest_binding(&manifest)?;
        if manifest.state() == HistoryOwnershipState::Migrating {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history cutover is in progress; staged data remains pending",
            ));
        }
        Ok(manifest)
    }

    fn validate_exact_write_manifest(&self, expected: &HistoryOwnershipManifest) -> io::Result<()> {
        self.validate_manifest_binding(expected)?;
        if expected.state() == HistoryOwnershipState::Migrating
            || self.ownership.load_manifest()?
                != OwnershipManifestStatus::Initialized(expected.clone())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "runtime history write authority became stale during the operation",
            ));
        }
        Ok(())
    }

    fn load_exact_v2_active_manifest(&self) -> io::Result<HistoryOwnershipManifest> {
        let manifest = match self.ownership.load_manifest()? {
            OwnershipManifestStatus::Uninitialized => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "v2 history is not initialized; explicit cutover is required",
                ));
            }
            OwnershipManifestStatus::Initialized(manifest) => manifest,
        };
        self.validate_manifest_binding(&manifest)?;
        if manifest.state() != HistoryOwnershipState::V2Active {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "v2 runtime history writes require a durable v2-active ownership state",
            ));
        }
        Ok(manifest)
    }

    fn validate_exact_active_manifest(
        &self,
        expected: &HistoryOwnershipManifest,
    ) -> io::Result<()> {
        self.validate_manifest_binding(expected)?;
        if expected.state() != HistoryOwnershipState::V2Active
            || self.ownership.load_manifest()?
                != OwnershipManifestStatus::Initialized(expected.clone())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "v2 runtime history authority became stale during the operation",
            ));
        }
        Ok(())
    }
}

fn v1_write_succeeded(report: &HistoryWriteReport) -> bool {
    !report.read_only && report.warnings.is_empty()
}

fn saturating_duration_us(duration: StdDuration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn bounded_garbage_collection_warning(error: &io::Error) -> String {
    const PREFIX: &str = "v2 history garbage collection failed: ";
    let normalized = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_GARBAGE_COLLECTION_WARNING_CHARS.saturating_sub(PREFIX.chars().count()))
        .collect::<String>();
    format!("{PREFIX}{normalized}")
}

fn strict_absolute_path(path: &Path, subject: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{subject} must be absolute"),
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{subject} must not contain . or .. components"),
        ));
    }
    Ok(path.to_path_buf())
}

fn canonical_existing_directory(path: &Path, subject: &str) -> io::Result<PathBuf> {
    let path = strict_absolute_path(path, subject)?;
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{subject} is not a directory"),
        ));
    }
    Ok(canonical)
}

fn validate_directory_leaf_if_present(path: &Path, subject: &str) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || is_windows_reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must not be a symlink or reparse point"),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{subject} is not a directory"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{
        ApiCostAmount, Confidence, Provenance, TaskRecord, TaskStatus, TokenUsage, UsageCall,
    };
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryObservation,
        LocalHalfHourBucket, LocalProjectUsageGroup, QuotaPoint,
    };
    use crate::history_query::HistorySourceSelectionStatus;
    use crate::local_history_migration::{
        LocalV1MigrationOptions, load_migrated_local_history_since, migrate_local_v1_history,
    };
    use crate::remote_protocol::{ProtocolRevisions, SourceGeneration};
    use crate::source_export::materialize_local_session_digest_evidence;
    use crate::source_history::{
        SourceBucketRecord, SourceHistoryRemoteBinding, SourceHistoryRemoteGenerationId,
        SourceKind, SourceMetadata, SourceSessionDigestChange,
    };
    use crate::source_identity::NodeId;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn runtime_paths(directory: &Path) -> (PathBuf, PathBuf) {
        let codex_home = directory.join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        (directory.join("state/history-v1"), codex_home)
    }

    fn runtime(directory: &Path, redact_content: bool) -> HistoryRuntime {
        let (history_root, codex_home) = runtime_paths(directory);
        HistoryRuntime::new_with_project_mapping_store(
            history_root,
            &codex_home,
            redact_content,
            ProjectMappingStore::new(directory.join("config/project-mappings.json")),
        )
        .unwrap()
    }

    fn sample_bucket(starts_at: DateTime<Utc>, tokens: u64) -> LocalHalfHourBucket {
        let ends_at = starts_at + Duration::minutes(15);
        LocalHalfHourBucket {
            starts_at,
            ends_at,
            sampled_at: ends_at,
            token_usage: TokenUsage {
                input_tokens: tokens,
                total_tokens: tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(tokens),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: Vec::new(),
            project_groups: vec![LocalProjectUsageGroup {
                thread_id: "thread-1".to_owned(),
                project_id: Some("project-1".to_owned()),
                project_label: Some("project".to_owned()),
                token_usage: TokenUsage {
                    input_tokens: tokens,
                    total_tokens: tokens,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::from(tokens),
                api_equivalent_cost: ApiCostAmount::default(),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            }],
            partial_reasons: Vec::new(),
        }
    }

    fn sample_observation(starts_at: DateTime<Utc>, tokens: u64) -> HistoryObservation {
        HistoryObservation {
            observed_at: starts_at + Duration::minutes(20),
            quota_points: vec![QuotaPoint {
                observed_at: starts_at + Duration::minutes(5),
                limit_id: "codex".to_owned(),
                duration_mins: 10_080,
                resets_at: starts_at + Duration::days(3),
                used_percent: 25.0,
                remaining_percent: 75.0,
                provenance: Provenance::ServerSnapshot,
            }],
            half_hour_buckets: vec![sample_bucket(starts_at, tokens)],
            weekly_local_points: Vec::new(),
        }
    }

    fn project_task(project: PathBuf) -> TaskRecord {
        TaskRecord {
            thread_id: "thread-1".to_owned(),
            parent_thread_id: None,
            archived: false,
            title: "task".to_owned(),
            cwd: Some(project),
            source: Some("cli".to_owned()),
            created_at: Some(at(30, 9, 0)),
            updated_at: Some(at(30, 9, 1)),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::High,
            api_equivalent_cost: None,
        }
    }

    fn initialized_manifest(runtime: &HistoryRuntime) -> HistoryOwnershipManifest {
        match runtime.ownership.load_manifest().unwrap() {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => panic!("ownership is uninitialized"),
        }
    }

    #[test]
    fn construction_binds_all_stores_without_initializing_ownership() {
        let directory = tempdir().unwrap();
        let runtime = runtime(directory.path(), false);

        assert_eq!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
        assert_eq!(runtime.source_history.state_root(), runtime.state_root());
        assert_eq!(runtime.ownership.state_root(), runtime.state_root());
        assert_eq!(runtime.source_history.profile_id(), runtime.profile_id());
        assert_eq!(
            runtime.redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
        assert!(!runtime.ownership.manifest_path().exists());
    }

    #[test]
    fn collection_observation_normalization_uses_private_source_project_identity() {
        let directory = tempdir().unwrap();
        let runtime = runtime(directory.path(), false);
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        let observation = sample_observation(at(30, 9, 0), 10);
        let task = TaskRecord {
            thread_id: "thread-1".to_owned(),
            parent_thread_id: None,
            archived: false,
            title: "task".to_owned(),
            cwd: Some(project),
            source: Some("cli".to_owned()),
            created_at: Some(at(30, 9, 0)),
            updated_at: Some(at(30, 9, 1)),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::High,
            api_equivalent_cost: None,
        };

        let normalized = runtime.normalize_local_collection_observation(&observation, &[task]);
        let project_id = normalized.half_hour_buckets[0].project_groups[0]
            .project_id
            .as_deref()
            .unwrap();
        assert!(project_id.starts_with("opk-hmac-sha256-v1-"));
        assert_ne!(project_id, "project-1");
        assert_eq!(
            normalized.half_hour_buckets[0].project_groups[0]
                .project_label
                .as_deref(),
            Some("project")
        );
        assert_eq!(
            observation.half_hour_buckets[0].project_groups[0]
                .project_id
                .as_deref(),
            Some("project-1")
        );
    }

    #[test]
    fn local_registration_deduplicates_same_path_and_logical_projection_survives_restart() {
        let directory = tempdir().unwrap();
        let mapping_store =
            ProjectMappingStore::new(directory.path().join("config/project-mappings.json"));
        let mut runtime = {
            let (history_root, codex_home) = runtime_paths(directory.path());
            HistoryRuntime::new_with_project_mapping_store(
                history_root,
                &codex_home,
                false,
                mapping_store.clone(),
            )
            .unwrap()
        };
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&project)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "config",
                    "--local",
                    "remote.origin.url",
                    "git@github.com:example/project.git",
                ])
                .current_dir(&project)
                .status()
                .unwrap()
                .success()
        );
        let observation = sample_observation(at(30, 9, 0), 10);
        let task = TaskRecord {
            thread_id: "thread-1".to_owned(),
            parent_thread_id: None,
            archived: false,
            title: "task".to_owned(),
            cwd: Some(project.clone()),
            source: Some("cli".to_owned()),
            created_at: Some(at(30, 9, 0)),
            updated_at: Some(at(30, 9, 1)),
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::High,
            api_equivalent_cost: None,
        };
        let mut alias_observation = observation.clone();
        let mut alias_group = alias_observation.half_hour_buckets[0].project_groups[0].clone();
        alias_group.thread_id = "thread-2".to_owned();
        alias_observation.half_hour_buckets[0]
            .project_groups
            .push(alias_group);
        let mut alias_task = task.clone();
        alias_task.thread_id = "thread-2".to_owned();
        let (aliases, registration) = runtime.normalize_and_register_local_collection_observation(
            &alias_observation,
            &[task.clone(), alias_task],
        );
        registration.unwrap();
        let alias_ids = aliases.half_hour_buckets[0]
            .project_groups
            .iter()
            .map(|group| group.project_id.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(alias_ids[0], alias_ids[1]);
        let mapped = mapping_store.load().unwrap();
        assert_eq!(mapped.instances().len(), 1);
        assert_eq!(mapped.instances()[0].observations().len(), 1);
        let encoded_mapping = serde_json::to_string(&mapped).unwrap();
        assert!(encoded_mapping.contains("git-sha256-v1-"));
        assert!(encoded_mapping.contains("\"repositoryRelativeWorkspaceRoot\":\".\""));
        assert!(!encoded_mapping.contains("github.com"));

        // The runtime cache is deliberately reused across collections. If a
        // second registration spawned Git again, hiding the repository here
        // would clear the stored evidence through metadata refresh.
        fs::rename(project.join(".git"), project.join(".git-hidden")).unwrap();
        runtime
            .normalize_and_register_local_collection_observation(
                &alias_observation,
                std::slice::from_ref(&task),
            )
            .1
            .unwrap();
        let cached_mapping = serde_json::to_string(&mapping_store.load().unwrap()).unwrap();
        assert!(cached_mapping.contains("git-sha256-v1-"));
        assert!(cached_mapping.contains("\"repositoryRelativeWorkspaceRoot\":\".\""));

        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        runtime
            .record_local_collection_observation(
                &observation,
                std::slice::from_ref(&task),
                LocalObservationMode::Incremental,
            )
            .unwrap();
        let mapped = mapping_store.load().unwrap();
        let instance_id = mapped.instances()[0].instance_id().clone();
        let merged = mapping_store
            .merge_instances(
                mapped.revision(),
                None,
                Some("logical workspace".parse().unwrap()),
                std::slice::from_ref(&instance_id),
            )
            .unwrap();
        let logical_id = merged.logical_project_id().clone();
        let first = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        let first_group = &first.history.half_hour_buckets[0].project_groups[0];
        assert_eq!(first_group.project_id.as_deref(), Some(logical_id.as_str()));
        assert_eq!(
            first_group.project_label.as_deref(),
            Some("logical workspace")
        );

        let history_root = directory.path().join("state/history-v1");
        let codex_home = directory.path().join("codex-home");
        drop(runtime);
        let mut restarted = HistoryRuntime::new_with_project_mapping_store(
            history_root,
            &codex_home,
            false,
            mapping_store,
        )
        .unwrap();
        let after_restart = restarted.load_unified_history_since(at(30, 8, 0)).unwrap();
        let restarted_group = &after_restart.history.half_hour_buckets[0].project_groups[0];
        assert_eq!(
            restarted_group.project_id.as_deref(),
            Some(logical_id.as_str())
        );
        assert_eq!(
            restarted_group.project_label.as_deref(),
            Some("logical workspace")
        );
    }

    #[test]
    fn local_mapping_failure_does_not_block_history_and_is_durably_visible() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let blocked_parent = directory.path().join("mapping-parent-is-a-file");
        fs::write(&blocked_parent, b"blocked").unwrap();
        let bad_mapping = ProjectMappingStore::new(blocked_parent.join("project-mappings.json"));
        let mut runtime = HistoryRuntime::new_with_project_mapping_store(
            history_root,
            &codex_home,
            false,
            bad_mapping,
        )
        .unwrap();
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        let observation = sample_observation(at(30, 9, 0), 10);

        runtime
            .record_local_collection_observation(
                &observation,
                &[project_task(project)],
                LocalObservationMode::Incremental,
            )
            .expect("mapping failure must not block history persistence");
        let queried = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(
            queried.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
        assert!(
            queried.history.half_hour_buckets[0]
                .partial_reasons
                .contains(&PROJECT_MAPPING_REGISTRATION_FAILED_WARNING.to_owned())
        );
        assert!(
            queried
                .history
                .warnings
                .contains(&PROJECT_MAPPING_REGISTRATION_FAILED_WARNING.to_owned())
        );
    }

    #[test]
    fn local_collection_session_digest_survives_runtime_restart_without_content() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let mapping_path = directory.path().join("config/project-mappings.json");
        let mut runtime = HistoryRuntime::new_with_project_mapping_store(
            history_root.clone(),
            &codex_home,
            false,
            ProjectMappingStore::new(mapping_path.clone()),
        )
        .unwrap();
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let project = directory.path().join("private-project-name");
        fs::create_dir(&project).unwrap();
        let task = project_task(project.clone());
        let observation = sample_observation(at(30, 9, 0), 100);
        let calls = vec![UsageCall {
            timestamp: at(30, 9, 5),
            thread_id: "thread-1".to_owned(),
            turn_id: Some("turn-private".to_owned()),
            usage_event_id: Some("native-local-event".to_owned()),
            usage_event_identity_exact: true,
            model: Some("gpt-5.6-luna".to_owned()),
            service_tier: Some("standard".to_owned()),
            tokens: TokenUsage {
                input_tokens: 100,
                total_tokens: 100,
                ..TokenUsage::default()
            },
            request_usage_exact: true,
        }];
        let evidence = materialize_local_session_digest_evidence(
            &calls,
            &observation.half_hour_buckets,
            observation.observed_at,
            false,
        )
        .unwrap();
        runtime
            .record_local_collection_with_session_digests(
                &observation,
                std::slice::from_ref(&task),
                &evidence,
                LocalObservationMode::Incremental,
            )
            .unwrap();
        let source_id = runtime.source_identity().node_id().clone();
        drop(runtime);

        let restarted = HistoryRuntime::new_with_project_mapping_store(
            history_root,
            &codex_home,
            false,
            ProjectMappingStore::new(mapping_path),
        )
        .unwrap();
        let records = restarted
            .source_history()
            .load_source_session_digest_records_since(
                &source_id,
                RedactionProfile::PreviewEnabled,
                at(30, 0, 0),
            )
            .unwrap();
        assert_eq!(records.records.len(), 1);
        let SourceSessionDigestChange::Upsert(digest) = records.records[0].change() else {
            panic!("the local digest must remain active after restart")
        };
        assert_eq!(digest.replica().thread_id().as_str(), "thread-1");
        assert_eq!(digest.metrics().token_usage.total_tokens, 100);
        assert_eq!(digest.observed_project_keys().len(), 1);
        let encoded = serde_json::to_string(&records.records).unwrap();
        assert!(!encoded.contains("private-project-name"));
        assert!(!encoded.contains("turn-private"));
        assert!(!encoded.contains(project.to_string_lossy().as_ref()));
    }

    #[test]
    fn ownership_initialization_never_advances_an_existing_phase() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);

        let v1 = runtime.ensure_ownership_initialized().unwrap();
        assert_eq!(v1.state(), HistoryOwnershipState::V1Active);
        assert_eq!(v1.epoch(), 1);
        assert_eq!(runtime.ensure_ownership_initialized().unwrap(), v1);

        let migrating = {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            match runtime.ownership.begin_migration(&lease, &v1).unwrap() {
                OwnershipCasOutcome::Applied(manifest) => manifest,
                OwnershipCasOutcome::Conflict(status) => {
                    panic!("unexpected migration conflict: {status:?}")
                }
            }
        };
        assert_eq!(migrating.state(), HistoryOwnershipState::Migrating);
        assert_eq!(runtime.ensure_ownership_initialized().unwrap(), migrating);

        let active = runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        assert_eq!(active.state(), HistoryOwnershipState::V2Active);
        assert_eq!(runtime.ensure_ownership_initialized().unwrap(), active);
    }

    #[test]
    fn local_runtime_writes_refuse_uninitialized_v1_and_migrating_phases() {
        let directory = tempdir().unwrap();
        let runtime = runtime(directory.path(), false);
        let observation = sample_observation(at(30, 9, 0), 101);

        let error = runtime
            .record_local_observation(&observation, LocalObservationMode::Incremental)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );

        let v1 = {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            match runtime.ownership.initialize_v1_active(&lease).unwrap() {
                InitializeV1Outcome::Initialized(manifest)
                | InitializeV1Outcome::Existing(manifest) => manifest,
            }
        };
        let error = runtime
            .record_local_observation(&observation, LocalObservationMode::Incremental)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            let outcome = runtime.ownership.begin_migration(&lease, &v1).unwrap();
            assert!(matches!(outcome, OwnershipCasOutcome::Applied(_)));
        }
        let error = runtime
            .record_local_observation(&observation, LocalObservationMode::Incremental)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let error = runtime.load_v2_summary_backfill_attempt().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn v2_local_runtime_writes_reserve_revisions_and_persist_marker() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();

        assert_eq!(runtime.load_v2_summary_backfill_attempt().unwrap(), None);
        let first = runtime
            .record_local_observation(
                &sample_observation(at(30, 9, 0), 111),
                LocalObservationMode::Incremental,
            )
            .unwrap();
        let second = runtime
            .record_local_observation(
                &sample_observation(at(30, 9, 15), 222),
                LocalObservationMode::Incremental,
            )
            .unwrap();
        assert_eq!(first.revision, 2);
        assert_eq!(second.revision, 3);

        let source = runtime
            .source_history
            .load_source_since(
                runtime.source_identity.node_id(),
                runtime.redaction_profile,
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(source.buckets.len(), 2);
        assert_eq!(source.buckets[0].token_usage.total_tokens, 111);
        assert_eq!(source.buckets[1].token_usage.total_tokens, 222);
        let metadata = runtime
            .source_history
            .load_source_metadata(runtime.source_identity.node_id())
            .unwrap();
        assert_eq!(metadata.kind(), crate::source_history::SourceKind::Local);
        assert_eq!(metadata.display_label(), LOCAL_SOURCE_LABEL);
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            runtime.redaction_profile
        );

        let partial = runtime
            .mark_v2_summary_backfill_attempt(at(30, 10, 0), false)
            .unwrap();
        assert_eq!(
            partial,
            V2SummaryBackfillAttempt {
                completed_at: at(30, 10, 0),
                complete: false,
            }
        );
        let complete = runtime
            .mark_v2_summary_backfill_attempt(at(30, 10, 1), true)
            .unwrap();
        assert!(complete.complete);
        assert_eq!(
            runtime.load_v2_summary_backfill_attempt().unwrap(),
            Some(complete)
        );
    }

    #[test]
    fn v2_gc_failure_is_reported_without_rolling_back_a_staged_observation() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let first = runtime
            .record_local_observation(
                &sample_observation(at(30, 9, 0), 111),
                LocalObservationMode::Incremental,
            )
            .unwrap();
        assert!(first.garbage_collection.attempted);
        assert!(first.garbage_collection.warning.is_none());

        // Corrupt an expired shard after the successful initial GC. The next
        // due pass must fail, but the observation preceding that best-effort
        // pass is already authoritative and must still clear the staged batch.
        let old = at(30, 9, 0) - Duration::days(40);
        let bucket_directory = runtime.source_history.source_buckets_directory(
            runtime.source_identity.node_id(),
            RedactionProfile::PreviewEnabled,
        );
        let invalid_shard = bucket_directory.join(format!("{}.json", old.format("%Y-%m-%d")));
        fs::write(&invalid_shard, b"not json\n").unwrap();

        let history_root = runtime
            .legacy_history()
            .history_root()
            .unwrap()
            .to_path_buf();
        let codex_home = directory.path().join("codex-home");
        drop(runtime);
        let mut restarted = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
        restarted.stage(&sample_observation(at(30, 15, 0), 222));
        let report = restarted.flush_staged().unwrap().unwrap();
        let HistoryRuntimeWriteReport::V2(report) = report else {
            panic!("expected a v2 write report");
        };
        assert_eq!(report.revision, 3);
        assert!(report.garbage_collection.attempted);
        let warning = report
            .garbage_collection
            .warning
            .as_deref()
            .expect("the failed retention pass must be visible to the caller");
        assert!(warning.starts_with("v2 history garbage collection failed:"));
        assert!(warning.chars().count() <= MAX_GARBAGE_COLLECTION_WARNING_CHARS);
        assert!(!warning.chars().any(char::is_control));
        assert!(restarted.flush_staged().unwrap().is_none());

        let unified = restarted.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert!(unified.history.warnings.iter().any(|item| item == warning));

        let source = restarted
            .source_history
            .load_source_since(
                restarted.source_identity.node_id(),
                restarted.redaction_profile,
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(source.buckets.len(), 2);
        assert_eq!(source.buckets[1].token_usage.total_tokens, 222);
    }

    #[test]
    fn v2_local_runtime_write_rejects_a_mismatched_source_store() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        runtime.source_history = SourceHistoryStore::new(
            directory.path().join("different-state-root"),
            runtime.profile_id.clone(),
        );

        let error = runtime
            .record_local_observation(
                &sample_observation(at(30, 9, 0), 111),
                LocalObservationMode::Incremental,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn redacted_runtime_direct_write_never_persists_plaintext_content() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), true);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let mut observation = sample_observation(at(30, 9, 0), 111);
        let group = &mut observation.half_hour_buckets[0].project_groups[0];
        group.title = Some("private incident title".to_owned());
        group.message_preview = Some("the private user request".to_owned());

        runtime
            .record_local_observation(&observation, LocalObservationMode::Incremental)
            .unwrap();
        let persisted = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        let group = &persisted.history.half_hour_buckets[0].project_groups[0];
        assert_eq!(group.title.as_deref(), Some("[redacted]"));
        assert_eq!(group.message_preview.as_deref(), Some("[redacted]"));
        assert_eq!(
            observation.half_hour_buckets[0].project_groups[0]
                .title
                .as_deref(),
            Some("private incident title")
        );
    }

    #[test]
    fn unified_query_requires_initialization_and_switches_backend_at_activation() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let starts_at = at(29, 9, 0);
        runtime
            .legacy
            .record(&sample_observation(starts_at, 123))
            .unwrap();

        let error = runtime
            .load_unified_history_since(at(29, 8, 0))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized,
            "a read must not initialize ownership implicitly"
        );

        let v1 = runtime.ensure_ownership_initialized().unwrap();
        let legacy = runtime.load_unified_history_since(at(29, 8, 0)).unwrap();
        assert_eq!(
            legacy.backend,
            crate::history_query::UnifiedHistoryBackend::V1
        );
        assert_eq!(legacy.ownership_epoch, v1.epoch());
        assert_eq!(legacy.history.half_hour_buckets.len(), 1);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let source_aware = runtime.load_unified_history_since(at(29, 8, 0)).unwrap();
        assert_eq!(
            source_aware.backend,
            crate::history_query::UnifiedHistoryBackend::V2
        );
        assert_eq!(source_aware.history.half_hour_buckets.len(), 1);
        assert_eq!(
            source_aware.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            123
        );
    }

    #[test]
    fn staged_overlay_and_flush_route_through_v1_and_v2() {
        let first_directory = tempdir().unwrap();
        let mut v1_runtime = runtime(first_directory.path(), false);
        v1_runtime.ensure_ownership_initialized().unwrap();
        v1_runtime.stage(&sample_observation(at(30, 9, 0), 111));

        let durable = v1_runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert!(durable.history.half_hour_buckets.is_empty());
        let live = v1_runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert_eq!(live.history.half_hour_buckets.len(), 1);
        assert!(matches!(
            v1_runtime.flush_staged_if_due(StdDuration::ZERO).unwrap(),
            Some(HistoryRuntimeWriteReport::V1(_))
        ));
        let durable = v1_runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(durable.history.half_hour_buckets.len(), 1);

        let second_directory = tempdir().unwrap();
        let mut v2_runtime = runtime(second_directory.path(), false);
        v2_runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        v2_runtime.stage(&sample_observation(at(30, 9, 0), 222));
        let durable = v2_runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert!(durable.history.half_hour_buckets.is_empty());
        let live = v2_runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert!(
            live.history.half_hour_buckets.is_empty(),
            "v2 must not apply a local replacement overlay to an additive aggregate"
        );
        let report = v2_runtime.flush_staged().unwrap();
        assert!(matches!(
            report,
            Some(HistoryRuntimeWriteReport::V2(LocalObservationWriteReport {
                revision: 2,
                ..
            }))
        ));
        let durable = v2_runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(durable.history.half_hour_buckets.len(), 1);
        assert_eq!(
            durable.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            222
        );
    }

    #[test]
    fn selected_v1_staged_overlay_is_limited_to_all_and_exact_local() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_ownership_initialized().unwrap();
        runtime.stage(&sample_observation(at(30, 9, 0), 111));
        let local_id = runtime.source_identity.node_id().clone();
        let remote_id: NodeId = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();

        let all = runtime
            .load_unified_history_since_with_staged_selected(
                &HistorySourceSelection::AllIncluded,
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(
            all.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(all.history.half_hour_buckets.len(), 1);

        let local = runtime
            .load_unified_history_since_with_staged_selected(
                &HistorySourceSelection::Local(local_id.clone()),
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(
            local.source_selection_status,
            HistorySourceSelectionStatus::Applied
        );
        assert_eq!(local.history.half_hour_buckets.len(), 1);
        assert_eq!(
            local.history.half_hour_buckets[0].token_usage.total_tokens,
            111
        );

        let remote = runtime
            .load_unified_history_since_with_staged_selected(
                &HistorySourceSelection::Remote(remote_id.clone()),
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(
            remote.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                crate::history_query::HistorySourceUnavailableReason::UnsupportedByLegacy
            )
        );
        assert!(remote.history.half_hour_buckets.is_empty());

        let stale_local = runtime
            .load_unified_history_since_with_staged_selected(
                &HistorySourceSelection::Local(remote_id),
                at(30, 8, 0),
            )
            .unwrap();
        assert_eq!(
            stale_local.source_selection_status,
            HistorySourceSelectionStatus::Unavailable(
                crate::history_query::HistorySourceUnavailableReason::LocalIdentityMismatch
            )
        );
        assert!(stale_local.history.half_hour_buckets.is_empty());
    }

    #[test]
    fn v2_staged_read_never_replaces_a_same_bucket_local_remote_aggregate() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let starts_at = at(30, 9, 0);
        runtime
            .record_local_observation(
                &sample_observation(starts_at, 10),
                LocalObservationMode::Incremental,
            )
            .unwrap();

        let remote_id: NodeId = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            let active = initialized_manifest(&runtime);
            let authority = runtime
                .ownership
                .authorize_v2_write(&lease, &active)
                .unwrap();
            let writer = runtime.source_history.writer(&authority).unwrap();
            writer
                .save_source_metadata(
                    &SourceMetadata::new_with_redaction_profile(
                        remote_id.clone(),
                        SourceKind::Ssh,
                        "remote",
                        runtime.redaction_profile,
                    )
                    .unwrap(),
                )
                .unwrap();
            let generation: SourceHistoryRemoteGenerationId =
                "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .parse()
                    .unwrap();
            let one = NonZeroU32::new(1).unwrap();
            let binding = SourceHistoryRemoteBinding::new(
                SourceGeneration {
                    node_id: remote_id.clone(),
                    generation: NonZeroU64::new(1).unwrap(),
                },
                ProtocolRevisions {
                    history_format: one,
                    metric: one,
                    estimator: one,
                    project_breakdown: one,
                    api_pricing_catalog: one,
                },
            )
            .unwrap();
            writer
                .ensure_remote_history_generation(
                    &remote_id,
                    runtime.redaction_profile,
                    &generation,
                    &binding,
                )
                .unwrap();
            writer
                .apply_remote_history_generation_page(
                    &remote_id,
                    runtime.redaction_profile,
                    &generation,
                    &binding,
                    &[SourceBucketRecord::upsert(1, {
                        // This test exercises staged local/remote composition,
                        // not replica de-duplication. Keep the remote fixture a
                        // distinct physical session so the missing-digest
                        // safety rule does not correctly select one authority.
                        let mut bucket = sample_bucket(starts_at, 20);
                        bucket.project_groups[0].thread_id = "thread-remote".to_owned();
                        bucket.project_groups[0].project_id = Some("project-remote".to_owned());
                        bucket
                    })
                    .unwrap()],
                    &[],
                )
                .unwrap();
            writer
                .activate_remote_history_generation(
                    &remote_id,
                    runtime.redaction_profile,
                    None,
                    &generation,
                    &binding,
                    at(30, 9, 20),
                )
                .unwrap();
        }

        runtime.stage(&sample_observation(starts_at, 15));
        let before_flush = runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert_eq!(before_flush.backend, UnifiedHistoryBackend::V2);
        assert_eq!(before_flush.history.half_hour_buckets.len(), 1);
        assert_eq!(
            before_flush.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            30,
            "the safe staged view must retain the durable remote slice exactly once"
        );
        assert_eq!(
            before_flush.history.half_hour_buckets[0]
                .project_groups
                .len(),
            2
        );

        runtime.flush_staged().unwrap();
        let after_flush = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(
            after_flush.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            35,
            "the newer local revision must replace local only and preserve remote once"
        );
    }

    #[test]
    fn migrating_and_failed_v2_flushes_preserve_staged_data() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let v1 = runtime.ensure_ownership_initialized().unwrap();
        runtime.stage(&sample_observation(at(30, 9, 0), 333));
        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            assert!(matches!(
                runtime.ownership.begin_migration(&lease, &v1).unwrap(),
                OwnershipCasOutcome::Applied(_)
            ));
        }

        let error = runtime.flush_staged().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let during = runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert_eq!(during.history.half_hour_buckets.len(), 1);

        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let correct_store = runtime.source_history.clone();
        runtime.source_history = SourceHistoryStore::new(
            directory.path().join("wrong-state-root"),
            runtime.profile_id.clone(),
        );
        let error = runtime.flush_staged().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        runtime.source_history = correct_store;

        let retained = runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert!(retained.history.half_hour_buckets.is_empty());
        assert!(matches!(
            runtime.flush_staged().unwrap(),
            Some(HistoryRuntimeWriteReport::V2(_))
        ));
        let durable = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(durable.history.half_hour_buckets.len(), 1);
    }

    #[test]
    fn an_old_successful_snapshot_cannot_clear_newer_staged_data() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        runtime.stage(&sample_observation(at(30, 9, 0), 444));
        let old = runtime.legacy.prepare_staged_flush().unwrap();

        runtime
            .record_local_observation(old.observation(), LocalObservationMode::Incremental)
            .unwrap();
        runtime.stage(&sample_observation(at(30, 9, 15), 555));
        assert!(!runtime.legacy.complete_staged_flush(&old));

        let durable_before_retry = runtime
            .load_unified_history_since_with_staged(at(30, 8, 0))
            .unwrap();
        assert_eq!(durable_before_retry.history.half_hour_buckets.len(), 1);
        assert!(matches!(
            runtime.flush_staged().unwrap(),
            Some(HistoryRuntimeWriteReport::V2(_))
        ));
        let live = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(live.history.half_hour_buckets.len(), 2);
        assert_eq!(
            live.history
                .half_hour_buckets
                .iter()
                .map(|bucket| bucket.token_usage.total_tokens)
                .sum::<u64>(),
            999
        );
    }

    #[test]
    fn full_staging_requires_explicit_v2_reconciliation() {
        let directory = tempdir().unwrap();
        let mut v2_runtime = runtime(directory.path(), false);
        v2_runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        let first = HistoryObservation {
            observed_at: at(30, 9, 40),
            half_hour_buckets: vec![
                sample_bucket(at(30, 9, 0), 100),
                sample_bucket(at(30, 9, 15), 200),
            ],
            ..HistoryObservation::default()
        };
        v2_runtime.stage(&first);
        v2_runtime.flush_staged().unwrap();

        let replacement = HistoryObservation {
            observed_at: at(30, 10, 0),
            half_hour_buckets: vec![sample_bucket(at(30, 9, 0), 150)],
            ..HistoryObservation::default()
        };
        v2_runtime.stage_full_observation(&replacement);
        let error = v2_runtime.flush_staged().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let report = v2_runtime
            .flush_staged_reconcile(at(30, 9, 0), at(30, 9, 30))
            .unwrap();
        let Some(HistoryRuntimeWriteReport::V2(report)) = report else {
            panic!("expected a v2 reconciliation report")
        };
        assert_eq!(report.bucket_tombstones, 1);
        let durable = v2_runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(durable.history.half_hour_buckets.len(), 1);
        assert_eq!(
            durable.history.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            150
        );

        let v1_directory = tempdir().unwrap();
        let mut v1_runtime = runtime(v1_directory.path(), false);
        v1_runtime.ensure_ownership_initialized().unwrap();
        v1_runtime.stage_full_observation(&replacement);
        assert!(matches!(
            v1_runtime
                .flush_staged_reconcile(at(30, 9, 0), at(30, 9, 30))
                .unwrap(),
            Some(HistoryRuntimeWriteReport::V1(_))
        ));
    }

    #[test]
    fn summary_marker_uses_active_backend_and_pending_state() {
        let directory = tempdir().unwrap();
        let mut v1_runtime = runtime(directory.path(), false);
        v1_runtime.ensure_ownership_initialized().unwrap();
        v1_runtime.stage(&sample_observation(at(30, 9, 0), 1));
        let partial = v1_runtime
            .mark_summary_backfill_attempt(at(30, 10, 0), true)
            .unwrap();
        assert!(!partial.complete);
        v1_runtime.flush_staged().unwrap();
        let complete = v1_runtime
            .mark_summary_backfill_attempt(at(30, 10, 1), true)
            .unwrap();
        assert!(complete.complete);

        let v2_directory = tempdir().unwrap();
        let mut v2_runtime = runtime(v2_directory.path(), false);
        v2_runtime.ensure_v2_active_at(at(30, 8, 0)).unwrap();
        v2_runtime.stage(&sample_observation(at(30, 9, 0), 2));
        let partial = v2_runtime
            .mark_summary_backfill_attempt(at(30, 10, 0), true)
            .unwrap();
        assert!(!partial.complete);
        v2_runtime.flush_staged().unwrap();
        let complete = v2_runtime
            .mark_summary_backfill_attempt(at(30, 10, 1), true)
            .unwrap();
        assert!(complete.complete);
        assert_eq!(
            v2_runtime.load_v2_summary_backfill_attempt().unwrap(),
            Some(V2SummaryBackfillAttempt {
                completed_at: complete.completed_at,
                complete: true,
            })
        );
    }

    #[test]
    fn empty_v1_migrates_and_v2_activation_is_idempotent() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);

        let first = runtime.ensure_v2_active_at(at(30, 12, 0)).unwrap();
        assert_eq!(first.state(), HistoryOwnershipState::V2Active);
        assert_eq!(first.epoch(), 2);
        // Exact V2Active is already beyond the irreversible cutover. It must
        // not query the global service manager or coordination directory on
        // every normal startup/read.
        runtime
            .set_service_coordination_root_for_test(directory.path().join("missing-parent/root"));
        runtime.service_definition_observation_override =
            Some(ServiceDefinitionObservation::Unverifiable(
                "this observation must not be consulted for V2Active".to_string(),
            ));
        let second = runtime.ensure_v2_active_at(at(30, 13, 0)).unwrap();
        assert_eq!(second, first);
        assert_eq!(initialized_manifest(&runtime), first);

        let migrated = load_migrated_local_history_since(
            &runtime.source_history,
            &runtime.ownership,
            &first,
            &runtime.source_identity,
            runtime.redaction_profile,
            migration_window_starts_at(),
        )
        .unwrap();
        assert!(migrated.account.quota_points.is_empty());
        assert!(migrated.source.buckets.is_empty());
    }

    #[test]
    fn existing_v1_data_is_imported_once() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let starts_at = at(20, 9, 0);
        runtime
            .legacy
            .record(&sample_observation(starts_at, 321))
            .unwrap();

        let active = runtime.ensure_v2_active_at(at(30, 12, 0)).unwrap();
        let migrated = load_migrated_local_history_since(
            &runtime.source_history,
            &runtime.ownership,
            &active,
            &runtime.source_identity,
            runtime.redaction_profile,
            migration_window_starts_at(),
        )
        .unwrap();
        assert_eq!(migrated.account.quota_points.len(), 1);
        assert_eq!(migrated.source.buckets.len(), 1);
        assert_eq!(migrated.source.buckets[0].token_usage.total_tokens, 321);

        assert_eq!(runtime.ensure_v2_active_at(at(30, 13, 0)).unwrap(), active);
        let reloaded = runtime
            .source_history
            .load_source_since(
                runtime.source_identity.node_id(),
                runtime.redaction_profile,
                migration_window_starts_at(),
            )
            .unwrap();
        assert_eq!(reloaded.buckets.len(), 1);
    }

    #[test]
    fn complete_migrating_state_recovers_after_crash_before_activation() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        runtime
            .legacy
            .record(&sample_observation(at(21, 10, 0), 444))
            .unwrap();

        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            let v1 = match runtime.ownership.initialize_v1_active(&lease).unwrap() {
                InitializeV1Outcome::Initialized(manifest)
                | InitializeV1Outcome::Existing(manifest) => manifest,
            };
            let migrating = match runtime.ownership.begin_migration(&lease, &v1).unwrap() {
                OwnershipCasOutcome::Applied(manifest) => manifest,
                OwnershipCasOutcome::Conflict(status) => {
                    panic!("unexpected migration conflict: {status:?}")
                }
            };
            let options = LocalV1MigrationOptions {
                source_identity: &runtime.source_identity,
                redaction_profile: runtime.redaction_profile,
                source_label: LOCAL_SOURCE_LABEL,
                expected_ownership_epoch: migrating.epoch(),
                window_starts_at: migration_window_starts_at(),
                completed_at: at(30, 12, 0),
            };
            migrate_local_v1_history(
                &mut runtime.legacy,
                &runtime.source_history,
                &runtime.ownership,
                &lease,
                &migrating,
                &options,
            )
            .unwrap();
            assert_eq!(
                initialized_manifest(&runtime).state(),
                HistoryOwnershipState::Migrating
            );
        }
        drop(runtime);

        let mut recovered = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
        let active = recovered.ensure_v2_active_at(at(30, 12, 5)).unwrap();
        assert_eq!(active.state(), HistoryOwnershipState::V2Active);
        let migrated = load_migrated_local_history_since(
            &recovered.source_history,
            &recovered.ownership,
            &active,
            &recovered.source_identity,
            recovered.redaction_profile,
            migration_window_starts_at(),
        )
        .unwrap();
        assert_eq!(migrated.source.buckets.len(), 1);
    }

    #[test]
    fn durable_service_blocker_prevents_the_central_cutover_state_machine() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let coordination_root = directory.path().join("current-user-service-scope");
        runtime.set_service_coordination_root_for_test(coordination_root.clone());
        let guard = match try_acquire_service_cutover_shared_at(&coordination_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test service gate was unexpectedly busy"),
        };
        drop(guard);
        let blocker = coordination_root.join("recorder-cutover-blocked.json");
        fs::write(&blocker, b"durable service replacement fence\n").unwrap();

        let error = runtime.ensure_v2_active_at(at(30, 12, 0)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
        fs::remove_file(blocker).unwrap();
        assert_eq!(
            runtime.ensure_v2_active_at(at(30, 12, 0)).unwrap().state(),
            HistoryOwnershipState::V2Active
        );
    }

    #[test]
    fn service_cannot_publish_a_blocker_after_cutover_passes_its_check() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let coordination_root = directory.path().join("current-user-service-scope");
        runtime.set_service_coordination_root_for_test(coordination_root.clone());
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume = std::sync::Arc::new(std::sync::Barrier::new(2));
        runtime.pause_cutover_after_blocker_check_for_test(entered.clone(), resume.clone());

        let cutover = std::thread::spawn(move || runtime.ensure_v2_active_at(at(30, 12, 0)));
        entered.wait();
        assert!(matches!(
            try_acquire_service_cutover_exclusive_at(&coordination_root).unwrap(),
            TryRecorderInstanceLock::Busy
        ));
        assert!(
            !coordination_root
                .join("recorder-cutover-blocked.json")
                .exists()
        );
        resume.wait();

        assert_eq!(
            cutover.join().unwrap().unwrap().state(),
            HistoryOwnershipState::V2Active
        );
    }

    #[test]
    fn one_global_service_blocker_covers_old_and_new_custom_history_roots() {
        let directory = tempdir().unwrap();
        let coordination_root = directory.path().join("current-user-service-scope");
        let old_root = directory.path().join("old-history-a");
        let new_root = directory.path().join("new-history-b");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&new_root).unwrap();
        let mut old_runtime = runtime(&old_root, false);
        let mut new_runtime = runtime(&new_root, false);
        old_runtime.set_service_coordination_root_for_test(coordination_root.clone());
        new_runtime.set_service_coordination_root_for_test(coordination_root.clone());
        let guard = match try_acquire_service_cutover_exclusive_at(&coordination_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test service gate was unexpectedly busy"),
        };
        fs::write(
            coordination_root.join("recorder-cutover-blocked.json"),
            b"cleanup of the old A registration was ambiguous\n",
        )
        .unwrap();
        drop(guard);

        assert_eq!(
            old_runtime
                .ensure_v2_active_at(at(30, 12, 0))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            new_runtime
                .ensure_v2_active_at(at(30, 12, 0))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            old_runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
        assert_eq!(
            new_runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
    }

    #[test]
    fn dormant_legacy_service_without_a_trusted_definition_marker_blocks_cutover() {
        let directory = tempdir().unwrap();
        let mut runtime = runtime(directory.path(), false);
        let coordination_root = directory.path().join("current-user-service-scope");
        runtime.set_service_coordination_root_for_test(coordination_root.clone());
        runtime.service_definition_observation_override = Some(
            ServiceDefinitionObservation::Fingerprint("legacy-v0.3-definition".to_string()),
        );

        let error = runtime.ensure_v2_active_at(at(30, 12, 0)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains("without a trusted current-version marker")
        );
        assert_eq!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Uninitialized
        );
    }

    #[test]
    fn migrating_state_without_a_marker_restarts_the_import() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let mut runtime = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        runtime
            .legacy
            .record(&sample_observation(at(22, 10, 0), 555))
            .unwrap();

        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            let v1 = match runtime.ownership.initialize_v1_active(&lease).unwrap() {
                InitializeV1Outcome::Initialized(manifest)
                | InitializeV1Outcome::Existing(manifest) => manifest,
            };
            let migrating = match runtime.ownership.begin_migration(&lease, &v1).unwrap() {
                OwnershipCasOutcome::Applied(manifest) => manifest,
                OwnershipCasOutcome::Conflict(status) => {
                    panic!("unexpected migration conflict: {status:?}")
                }
            };
            assert_eq!(migrating.state(), HistoryOwnershipState::Migrating);
            // Simulate a crash after the ownership CAS but before the first
            // durable migration marker is written.
        }
        drop(runtime);

        let mut recovered = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
        let active = recovered.ensure_v2_active_at(at(30, 12, 5)).unwrap();
        assert_eq!(active.state(), HistoryOwnershipState::V2Active);
        let migrated = load_migrated_local_history_since(
            &recovered.source_history,
            &recovered.ownership,
            &active,
            &recovered.source_identity,
            recovered.redaction_profile,
            migration_window_starts_at(),
        )
        .unwrap();
        assert_eq!(migrated.source.buckets.len(), 1);
        assert_eq!(migrated.source.buckets[0].token_usage.total_tokens, 555);
    }

    #[test]
    fn migrating_recovery_still_honors_the_global_service_blocker() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let mut runtime = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
        {
            let lease = runtime.ownership.acquire_writer_lease().unwrap();
            let v1 = match runtime.ownership.initialize_v1_active(&lease).unwrap() {
                InitializeV1Outcome::Initialized(manifest)
                | InitializeV1Outcome::Existing(manifest) => manifest,
            };
            assert!(matches!(
                runtime.ownership.begin_migration(&lease, &v1).unwrap(),
                OwnershipCasOutcome::Applied(_)
            ));
        }
        let coordination_root = directory.path().join("current-user-service-scope");
        runtime.set_service_coordination_root_for_test(coordination_root.clone());
        let guard = match try_acquire_service_cutover_exclusive_at(&coordination_root).unwrap() {
            TryRecorderInstanceLock::Acquired(guard) => guard,
            TryRecorderInstanceLock::Busy => panic!("test service gate was unexpectedly busy"),
        };
        fs::write(
            coordination_root.join("recorder-cutover-blocked.json"),
            b"service replacement is unresolved\n",
        )
        .unwrap();
        drop(guard);

        assert_eq!(
            runtime
                .ensure_v2_active_at(at(30, 12, 5))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            runtime.ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(manifest)
                if manifest.state() == HistoryOwnershipState::Migrating
        ));
    }

    #[test]
    fn concurrent_ensure_calls_converge_on_one_activation() {
        let directory = tempdir().unwrap();
        let (history_root, codex_home) = runtime_paths(directory.path());
        let first = HistoryRuntime::new(history_root.clone(), &codex_home, false).unwrap();
        let second = HistoryRuntime::new(history_root, &codex_home, false).unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let run = |mut runtime: HistoryRuntime, barrier: Arc<Barrier>| {
            thread::spawn(move || {
                barrier.wait();
                runtime.ensure_v2_active_at(at(30, 12, 0))
            })
        };
        let first_handle = run(first, Arc::clone(&barrier));
        let second_handle = run(second, barrier);
        let first = first_handle.join().unwrap().unwrap();
        let second = second_handle.join().unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.state(), HistoryOwnershipState::V2Active);
        assert_eq!(first.epoch(), 2);
    }

    #[test]
    fn redacted_runtime_derives_the_unredacted_profile_id() {
        let directory = tempdir().unwrap();
        let runtime = runtime(directory.path(), true);
        assert_eq!(runtime.redaction_profile(), RedactionProfile::Redacted);
        assert_eq!(
            runtime.legacy.namespace(),
            format!("{}-redacted", runtime.profile_id())
        );
    }

    #[test]
    fn custom_layout_and_relative_roots_fail_before_ownership_initialization() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();

        let wrong_leaf = directory.path().join("state/custom-history");
        let error = HistoryRuntime::new(wrong_leaf, &codex_home, false)
            .err()
            .expect("custom leaf should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!directory.path().join("state").exists());

        let error = HistoryRuntime::new(PathBuf::from("history-v1"), &codex_home, false)
            .err()
            .expect("relative root should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_legacy_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let state_root = directory.path().join("state");
        let actual = directory.path().join("actual-history");
        fs::create_dir(&codex_home).unwrap();
        fs::create_dir(&state_root).unwrap();
        fs::create_dir(&actual).unwrap();
        symlink(&actual, state_root.join(LEGACY_HISTORY_DIRECTORY)).unwrap();

        let error = HistoryRuntime::new(
            state_root.join(LEGACY_HISTORY_DIRECTORY),
            &codex_home,
            false,
        )
        .err()
        .expect("symlinked history root should fail");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_codex_home_preserves_the_legacy_namespace_identity() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let actual_codex_home = directory.path().join("actual-codex-home");
        let codex_home_alias = directory.path().join("codex-home-alias");
        let history_root = directory.path().join("state/history-v1");
        fs::create_dir(&actual_codex_home).unwrap();
        symlink(&actual_codex_home, &codex_home_alias).unwrap();

        // v0.3 HistoryStore normalizes an existing Codex home through
        // canonicalize before hashing it. HistoryRuntime's stricter directory
        // validation must retain that exact namespace rather than creating an
        // empty sibling profile for the symlink spelling.
        let mut legacy =
            HistoryStore::new_with_redaction(history_root.clone(), &codex_home_alias, false);
        legacy
            .record(&sample_observation(at(30, 9, 0), 4321))
            .unwrap();
        let mut runtime = HistoryRuntime::new(history_root, &codex_home_alias, false).unwrap();
        assert_eq!(runtime.legacy.namespace(), legacy.namespace());
        assert_eq!(
            fs::canonicalize(runtime.legacy.namespace_dir().unwrap()).unwrap(),
            fs::canonicalize(legacy.namespace_dir().unwrap()).unwrap(),
            "the migration reader must target the existing v0.3 namespace directory"
        );
        runtime.ensure_ownership_initialized().unwrap();
        let loaded = runtime.load_unified_history_since(at(30, 8, 0)).unwrap();
        assert_eq!(loaded.history.half_hour_buckets.len(), 1);
        assert_eq!(
            loaded.history.half_hour_buckets[0].token_usage.total_tokens,
            4321
        );
    }
}
