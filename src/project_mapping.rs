//! Durable, center-owned mapping from source project observations to logical
//! projects.
//!
//! Observations are always scoped by [`NodeId`]. Merely seeing the same label
//! or Git evidence on two sources never changes aggregation identity: those
//! signals are retained only so callers can render deterministic merge
//! suggestions. All aliases and logical merges require an explicit CAS
//! mutation.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Deserializer, Serialize};

use crate::atomic_file::replace_file;
use crate::remote_protocol::{
    GitRepositoryFingerprint, RemoteGitRepositoryEvidence, RemoteProjectDescriptor,
};
use crate::source_identity::NodeId;
use crate::source_model::{
    LogicalProjectId, ObservedProjectKey, ProjectDisplayLabel, ProjectInstanceId,
};

pub const PROJECT_MAPPING_VERSION: u32 = 2;
pub const PROJECT_MAPPING_REGISTRATION_FAILED_WARNING: &str = "project_mapping_registration_failed";

const APP_DIRECTORY: &str = "codex-usage-monit";
const CONFIG_DIRECTORY_ENV: &str = "CODEX_USAGE_MONIT_CONFIG_DIR";
const MAPPING_FILE: &str = "project-mappings.json";
const LOCK_FILE: &str = "project-mappings.lock";
const MAX_MAPPING_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INSTANCES: usize = 32_768;
const MAX_LOGICAL_PROJECTS: usize = 32_768;
const MAX_OBSERVATIONS_PER_INSTANCE: usize = 256;
const MAX_TOTAL_OBSERVATIONS: usize = 131_072;
const MAX_REPOSITORY_RELATIVE_ROOT_BYTES: usize = 4_096;
const TEMP_FILE_ATTEMPTS: usize = 128;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A project observation is only unique together with its source identity.
///
/// Although an observed key already incorporates a source-owned secret, the
/// explicit source dimension prevents imported, malformed, or rotated-source
/// data from collapsing into another machine's namespace.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceObservedProject {
    source_id: NodeId,
    observed_project_key: ObservedProjectKey,
}

impl SourceObservedProject {
    pub fn new(source_id: NodeId, observed_project_key: ObservedProjectKey) -> Self {
        Self {
            source_id,
            observed_project_key,
        }
    }

    pub fn source_id(&self) -> &NodeId {
        &self.source_id
    }

    pub fn observed_project_key(&self) -> &ObservedProjectKey {
        &self.observed_project_key
    }
}

impl PartialOrd for SourceObservedProject {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for SourceObservedProject {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.source_id
            .as_str()
            .cmp(other.source_id.as_str())
            .then_with(|| {
                self.observed_project_key
                    .as_str()
                    .cmp(other.observed_project_key.as_str())
            })
    }
}

/// Sanitized observation metadata retained for display and suggestions.
/// None of these fields participates in identity or automatic merging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectObservation {
    identity: SourceObservedProject,
    display_label: Option<ProjectDisplayLabel>,
    git_evidence: RemoteGitRepositoryEvidence,
}

impl ProjectObservation {
    pub fn new(identity: SourceObservedProject) -> Self {
        Self {
            identity,
            display_label: None,
            git_evidence: RemoteGitRepositoryEvidence::Unavailable,
        }
    }

    pub fn from_remote_descriptor(
        source_id: NodeId,
        descriptor: &RemoteProjectDescriptor,
    ) -> io::Result<Self> {
        Self::new(SourceObservedProject::new(
            source_id,
            descriptor.observed_project_key.clone(),
        ))
        .with_display_label(Some(descriptor.display_label.clone()))
        .with_git_evidence_state(descriptor.git_evidence.clone())
    }

    pub fn identity(&self) -> &SourceObservedProject {
        &self.identity
    }

    pub fn display_label(&self) -> Option<&ProjectDisplayLabel> {
        self.display_label.as_ref()
    }

    pub fn git_repository_fingerprint(&self) -> Option<&GitRepositoryFingerprint> {
        self.git_evidence.fingerprint()
    }

    pub fn repository_relative_workspace_root(&self) -> Option<&str> {
        self.git_evidence.repository_relative_workspace_root()
    }

    pub fn with_display_label(mut self, display_label: Option<ProjectDisplayLabel>) -> Self {
        self.display_label = display_label;
        self
    }

    pub fn with_git_evidence(
        self,
        git_repository_fingerprint: Option<GitRepositoryFingerprint>,
        repository_relative_workspace_root: Option<String>,
    ) -> io::Result<Self> {
        let git_evidence = match (
            git_repository_fingerprint,
            repository_relative_workspace_root,
        ) {
            (None, None) => RemoteGitRepositoryEvidence::Unavailable,
            (fingerprint, Some(repository_relative_workspace_root)) => {
                RemoteGitRepositoryEvidence::Repository {
                    fingerprint,
                    repository_relative_workspace_root,
                }
            }
            (Some(_), None) => {
                return Err(invalid_mapping(
                    "Git fingerprint requires a repository-relative workspace root",
                ));
            }
        };
        self.with_git_evidence_state(git_evidence)
    }

    pub fn with_git_evidence_state(
        mut self,
        git_evidence: RemoteGitRepositoryEvidence,
    ) -> io::Result<Self> {
        validate_git_evidence(&git_evidence)?;
        self.git_evidence = git_evidence;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredObservation {
    identity: SourceObservedProject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_label: Option<ProjectDisplayLabel>,
    git_evidence: RemoteGitRepositoryEvidence,
}

impl StoredObservation {
    fn from_observation(observation: ProjectObservation) -> io::Result<Self> {
        validate_git_evidence(&observation.git_evidence)?;
        Ok(Self {
            identity: observation.identity,
            display_label: observation.display_label,
            git_evidence: observation.git_evidence,
        })
    }

    fn refresh_from(&mut self, observation: ProjectObservation) -> io::Result<bool> {
        let mut replacement = Self::from_observation(observation)?;
        if self.identity != replacement.identity {
            return Err(invalid_mapping(
                "observation metadata cannot change its source-scoped identity",
            ));
        }
        // Only an explicitly unavailable probe preserves prior evidence.
        // ConfirmedNonRepository clears both fields, while Repository with no
        // fingerprint keeps its root and clears a previously observed origin.
        if matches!(
            replacement.git_evidence,
            RemoteGitRepositoryEvidence::Unavailable
        ) {
            replacement.git_evidence = self.git_evidence.clone();
        }
        if self == &replacement {
            return Ok(false);
        }
        *self = replacement;
        Ok(true)
    }
}

/// One physical project instance. Multiple observations only appear here
/// after an explicit alias operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectInstanceMapping {
    instance_id: ProjectInstanceId,
    observations: Vec<StoredObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logical_project_id: Option<LogicalProjectId>,
}

impl ProjectInstanceMapping {
    pub fn instance_id(&self) -> &ProjectInstanceId {
        &self.instance_id
    }

    pub fn logical_project_id(&self) -> Option<&LogicalProjectId> {
        self.logical_project_id.as_ref()
    }

    pub fn observations(&self) -> impl ExactSizeIterator<Item = &SourceObservedProject> {
        self.observations.iter().map(|entry| &entry.identity)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalProjectMapping {
    logical_project_id: LogicalProjectId,
    display_label: ProjectDisplayLabel,
}

impl LogicalProjectMapping {
    pub fn logical_project_id(&self) -> &LogicalProjectId {
        &self.logical_project_id
    }

    pub fn display_label(&self) -> &ProjectDisplayLabel {
        &self.display_label
    }
}

/// Validated immutable snapshot of the complete mapping configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectMappings {
    version: u32,
    revision: u64,
    instances: Vec<ProjectInstanceMapping>,
    logical_projects: Vec<LogicalProjectMapping>,
}

impl Default for ProjectMappings {
    fn default() -> Self {
        Self {
            version: PROJECT_MAPPING_VERSION,
            revision: 0,
            instances: Vec::new(),
            logical_projects: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectMappingsWire {
    version: u32,
    revision: u64,
    instances: Vec<ProjectInstanceMapping>,
    logical_projects: Vec<LogicalProjectMapping>,
}

impl<'de> Deserialize<'de> for ProjectMappings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProjectMappingsWire::deserialize(deserializer)?;
        let mappings = Self {
            version: wire.version,
            revision: wire.revision,
            instances: wire.instances,
            logical_projects: wire.logical_projects,
        };
        mappings.validate().map_err(serde::de::Error::custom)?;
        Ok(mappings)
    }
}

impl ProjectMappings {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn instances(&self) -> &[ProjectInstanceMapping] {
        &self.instances
    }

    pub fn logical_projects(&self) -> &[LogicalProjectMapping] {
        &self.logical_projects
    }

    pub fn instance(&self, id: &ProjectInstanceId) -> Option<&ProjectInstanceMapping> {
        self.instances
            .binary_search_by(|entry| entry.instance_id.cmp(id))
            .ok()
            .map(|index| &self.instances[index])
    }

    pub fn logical_project(&self, id: &LogicalProjectId) -> Option<&LogicalProjectMapping> {
        self.logical_projects
            .binary_search_by(|entry| entry.logical_project_id.cmp(id))
            .ok()
            .map(|index| &self.logical_projects[index])
    }

    pub fn instance_for_observation(
        &self,
        identity: &SourceObservedProject,
    ) -> Option<&ProjectInstanceMapping> {
        self.instances.iter().find(|instance| {
            instance
                .observations
                .binary_search_by(|entry| entry.identity.cmp(identity))
                .is_ok()
        })
    }

    /// Builds a detached, read-only index. Applying it to history cannot
    /// mutate or discover mappings, so repeating a query is deterministic.
    pub fn projection(&self) -> ProjectMappingProjection {
        ProjectMappingProjection::from_mappings(self)
    }

    /// Recomputes non-authoritative merge suggestions from retained evidence.
    /// Suggestions never alter aggregate identity.
    pub fn merge_suggestions(&self) -> Vec<ProjectMergeSuggestion> {
        let mut git_groups = BTreeMap::<GitSuggestionKey, BTreeSet<ProjectInstanceId>>::new();

        for instance in &self.instances {
            for observation in &instance.observations {
                // A fingerprint without an explicit repository-relative root
                // cannot distinguish a repository root from missing monorepo
                // evidence. Keep it for display/search, but do not suggest a
                // merge until the wire model can represent that distinction.
                if let RemoteGitRepositoryEvidence::Repository {
                    fingerprint: Some(fingerprint),
                    repository_relative_workspace_root,
                } = &observation.git_evidence
                {
                    git_groups
                        .entry(GitSuggestionKey {
                            fingerprint: fingerprint.clone(),
                            repository_relative_workspace_root: repository_relative_workspace_root
                                .clone(),
                        })
                        .or_default()
                        .insert(instance.instance_id.clone());
                }
            }
        }

        let mut result = Vec::new();
        for (key, instances) in git_groups {
            if self.suggestion_changes_mapping(&instances) {
                result.push(ProjectMergeSuggestion {
                    reason: ProjectMergeSuggestionReason::MatchingGit {
                        fingerprint: key.fingerprint,
                        repository_relative_workspace_root: key.repository_relative_workspace_root,
                    },
                    instance_ids: instances.into_iter().collect(),
                });
            }
        }
        result.sort();
        result
    }

    fn suggestion_changes_mapping(&self, instances: &BTreeSet<ProjectInstanceId>) -> bool {
        if instances.len() < 2 {
            return false;
        }
        let mut aggregate_keys = BTreeSet::new();
        for id in instances {
            let Some(instance) = self.instance(id) else {
                return false;
            };
            aggregate_keys.insert(match &instance.logical_project_id {
                Some(logical) => ProjectAggregateId::Logical(logical.clone()),
                None => ProjectAggregateId::Instance(instance.instance_id.clone()),
            });
        }
        aggregate_keys.len() > 1
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != PROJECT_MAPPING_VERSION {
            return Err(invalid_mapping(format!(
                "unsupported project mapping version {}; expected {PROJECT_MAPPING_VERSION}",
                self.version
            )));
        }
        if self.instances.len() > MAX_INSTANCES {
            return Err(invalid_mapping(format!(
                "project mapping has too many instances; maximum is {MAX_INSTANCES}"
            )));
        }
        if self.logical_projects.len() > MAX_LOGICAL_PROJECTS {
            return Err(invalid_mapping(format!(
                "project mapping has too many logical projects; maximum is {MAX_LOGICAL_PROJECTS}"
            )));
        }

        let mut previous_instance: Option<&ProjectInstanceId> = None;
        let mut all_observations = HashSet::new();
        let mut total_observations = 0_usize;
        for instance in &self.instances {
            if previous_instance.is_some_and(|previous| previous >= &instance.instance_id) {
                return Err(invalid_mapping(
                    "project instances must be strictly ordered and unique",
                ));
            }
            previous_instance = Some(&instance.instance_id);
            if instance.observations.is_empty()
                || instance.observations.len() > MAX_OBSERVATIONS_PER_INSTANCE
            {
                return Err(invalid_mapping(format!(
                    "each project instance must have 1 to {MAX_OBSERVATIONS_PER_INSTANCE} observations"
                )));
            }
            let mut previous_observation: Option<&SourceObservedProject> = None;
            let mut instance_source_id: Option<&NodeId> = None;
            for observation in &instance.observations {
                if previous_observation.is_some_and(|previous| previous >= &observation.identity) {
                    return Err(invalid_mapping(
                        "project observations must be strictly ordered and unique",
                    ));
                }
                previous_observation = Some(&observation.identity);
                match instance_source_id {
                    Some(source_id) if source_id != observation.identity.source_id() => {
                        return Err(invalid_mapping(
                            "a physical project instance cannot contain observations from multiple sources",
                        ));
                    }
                    None => instance_source_id = Some(observation.identity.source_id()),
                    Some(_) => {}
                }
                if !all_observations.insert(observation.identity.clone()) {
                    return Err(invalid_mapping(
                        "a source project observation belongs to more than one instance",
                    ));
                }
                validate_git_evidence(&observation.git_evidence)?;
                total_observations = total_observations
                    .checked_add(1)
                    .ok_or_else(|| invalid_mapping("project observation count overflowed"))?;
            }
        }
        if total_observations > MAX_TOTAL_OBSERVATIONS {
            return Err(invalid_mapping(format!(
                "project mapping has too many observations; maximum is {MAX_TOTAL_OBSERVATIONS}"
            )));
        }

        let mut previous_logical: Option<&LogicalProjectId> = None;
        let mut logical_ids = HashSet::new();
        for logical in &self.logical_projects {
            if previous_logical.is_some_and(|previous| previous >= &logical.logical_project_id) {
                return Err(invalid_mapping(
                    "logical projects must be strictly ordered and unique",
                ));
            }
            previous_logical = Some(&logical.logical_project_id);
            logical_ids.insert(logical.logical_project_id.clone());
        }

        let mut referenced = HashSet::new();
        for instance in &self.instances {
            if let Some(logical) = &instance.logical_project_id {
                if !logical_ids.contains(logical) {
                    return Err(invalid_mapping(
                        "project instance references an unknown logical project",
                    ));
                }
                referenced.insert(logical.clone());
            }
        }
        if referenced.len() != logical_ids.len() {
            return Err(invalid_mapping(
                "logical projects without any member instance are not canonical",
            ));
        }
        Ok(())
    }

    fn advance_revision(&mut self) -> io::Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_mapping("project mapping revision overflowed"))?;
        Ok(())
    }

    fn remove_empty_logical_projects(&mut self) {
        let referenced = self
            .instances
            .iter()
            .filter_map(|instance| instance.logical_project_id.clone())
            .collect::<HashSet<_>>();
        self.logical_projects
            .retain(|logical| referenced.contains(&logical.logical_project_id));
    }
}

/// Deterministically chooses a label for an explicit new logical project.
///
/// This is shared by the TUI and CLI-facing callers so a manual merge does
/// not need a separate prompt. The lowest sanitized observed label wins;
/// unlabeled instances fall back to a stable short physical-instance ID.
pub fn manual_merge_display_label(
    mappings: &ProjectMappings,
    instance_ids: &[ProjectInstanceId],
) -> io::Result<ProjectDisplayLabel> {
    let unique = instance_ids.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() < 2 || unique.len() != instance_ids.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a manual project merge requires at least two unique instances",
        ));
    }
    let mut labels = Vec::new();
    for instance_id in &unique {
        let instance = mappings.instance(instance_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "project instance is not mapped")
        })?;
        if instance.logical_project_id.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "split an explicitly mapped instance before using it in a manual merge",
            ));
        }
        labels.extend(
            instance
                .observations
                .iter()
                .filter_map(|observation| observation.display_label.clone()),
        );
    }
    if let Some(label) = labels.into_iter().min() {
        return Ok(label);
    }
    let fallback = unique
        .first()
        .expect("a validated manual merge has at least two members")
        .as_str()
        .strip_prefix("project-instance-")
        .unwrap_or_else(|| {
            unique
                .first()
                .expect("a validated manual merge has at least two members")
                .as_str()
        })
        .chars()
        .take(8)
        .collect::<String>();
    fallback.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("generated manual project label is invalid: {error}"),
        )
    })
}

/// Aggregate identity returned to Summary/Trends consumers.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectAggregateId {
    Instance(ProjectInstanceId),
    Logical(LogicalProjectId),
}

impl ProjectAggregateId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Instance(id) => id.as_str(),
            Self::Logical(id) => id.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectProjection {
    instance_id: ProjectInstanceId,
    aggregate_id: ProjectAggregateId,
    display_label: Option<ProjectDisplayLabel>,
}

impl ProjectProjection {
    pub fn instance_id(&self) -> &ProjectInstanceId {
        &self.instance_id
    }

    pub fn aggregate_id(&self) -> &ProjectAggregateId {
        &self.aggregate_id
    }

    pub fn display_label(&self) -> Option<&ProjectDisplayLabel> {
        self.display_label.as_ref()
    }
}

/// Detached pure-query index. It contains no store path or mutation methods.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectMappingProjection {
    observations: BTreeMap<SourceObservedProject, ProjectProjection>,
}

impl ProjectMappingProjection {
    fn from_mappings(mappings: &ProjectMappings) -> Self {
        let logical_labels = mappings
            .logical_projects
            .iter()
            .map(|entry| {
                (
                    entry.logical_project_id.clone(),
                    entry.display_label.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut observations = BTreeMap::new();
        for instance in &mappings.instances {
            let instance_label = instance
                .observations
                .iter()
                .filter_map(|observation| observation.display_label.clone())
                .min();
            for observation in &instance.observations {
                let (aggregate_id, display_label) = match &instance.logical_project_id {
                    Some(logical) => (
                        ProjectAggregateId::Logical(logical.clone()),
                        logical_labels.get(logical).cloned(),
                    ),
                    None => (
                        ProjectAggregateId::Instance(instance.instance_id.clone()),
                        instance_label.clone(),
                    ),
                };
                observations.insert(
                    observation.identity.clone(),
                    ProjectProjection {
                        instance_id: instance.instance_id.clone(),
                        aggregate_id,
                        display_label,
                    },
                );
            }
        }
        Self { observations }
    }

    pub fn resolve(
        &self,
        source_id: &NodeId,
        observed_project_key: &ObservedProjectKey,
    ) -> Option<&ProjectProjection> {
        self.observations.get(&SourceObservedProject::new(
            source_id.clone(),
            observed_project_key.clone(),
        ))
    }

    pub fn len(&self) -> usize {
        self.observations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct GitSuggestionKey {
    fingerprint: GitRepositoryFingerprint,
    repository_relative_workspace_root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectMergeSuggestionReason {
    MatchingGit {
        fingerprint: GitRepositoryFingerprint,
        repository_relative_workspace_root: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectMergeSuggestion {
    reason: ProjectMergeSuggestionReason,
    instance_ids: Vec<ProjectInstanceId>,
}

impl ProjectMergeSuggestion {
    pub fn reason(&self) -> &ProjectMergeSuggestionReason {
        &self.reason
    }

    pub fn instance_ids(&self) -> &[ProjectInstanceId] {
        &self.instance_ids
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationResolution {
    mappings: ProjectMappings,
    instance_id: ProjectInstanceId,
    created: bool,
}

/// One atomic resolve-or-create result for an ordered descriptor batch.
///
/// `instance_ids` preserves the input order. A batch that creates or refreshes
/// any number of observations advances the mapping revision exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationBatchResolution {
    mappings: ProjectMappings,
    instance_ids: Vec<ProjectInstanceId>,
    created_count: usize,
}

impl ObservationBatchResolution {
    pub fn mappings(&self) -> &ProjectMappings {
        &self.mappings
    }

    pub fn into_mappings(self) -> ProjectMappings {
        self.mappings
    }

    pub fn instance_ids(&self) -> &[ProjectInstanceId] {
        &self.instance_ids
    }

    pub fn created_count(&self) -> usize {
        self.created_count
    }
}

impl ObservationResolution {
    pub fn mappings(&self) -> &ProjectMappings {
        &self.mappings
    }

    pub fn into_mappings(self) -> ProjectMappings {
        self.mappings
    }

    pub fn instance_id(&self) -> &ProjectInstanceId {
        &self.instance_id
    }

    pub fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalMergeResult {
    mappings: ProjectMappings,
    logical_project_id: LogicalProjectId,
    created: bool,
}

impl LogicalMergeResult {
    pub fn mappings(&self) -> &ProjectMappings {
        &self.mappings
    }

    pub fn logical_project_id(&self) -> &LogicalProjectId {
        &self.logical_project_id
    }

    pub fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug)]
pub struct ProjectMappingStore {
    path: Option<PathBuf>,
}

/// A fully serialized project-mapping candidate prepared under the mapping
/// lock. Expensive descriptor resolution, JSON encoding, file writes, and
/// file fsync all happen before this value is returned. Publishing therefore
/// needs only one atomic replacement while the caller holds its short control
/// plane fence.
#[derive(Debug)]
pub(crate) struct PreparedProjectMappingBatch {
    path: PathBuf,
    parent: PathBuf,
    temporary: Option<PathBuf>,
    expected_file: Option<File>,
}

/// Post-publication durability work deliberately finished after the remotes
/// config fence is released. Publication itself performs a nonblocking mapping
/// file-identity CAS; validation and directory fsync do not need to retain either
/// the mapping or remotes control-plane lock.
#[derive(Debug)]
pub(crate) struct PublishedProjectMappingBatch {
    path: PathBuf,
    parent: PathBuf,
    durability_required: bool,
}

impl PreparedProjectMappingBatch {
    pub(crate) fn publish(mut self) -> io::Result<PublishedProjectMappingBatch> {
        // Every nonempty descriptor batch carries an expected file. Retrying
        // validation and the parent-directory fsync even when its logical
        // contents are unchanged closes this crash boundary: a prior rename
        // may have reached the namespace but failed before its directory sync.
        let durability_required = self.expected_file.is_some();
        if let Some(expected_file) = self.expected_file.as_ref() {
            let Some(_lock) = try_open_locked_lock_file(&self.parent, LockMode::Exclusive)? else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "project mapping is busy; retry the staged remote page",
                ));
            };
            ensure_expected_mapping_file(&self.path, expected_file)?;
            if let Some(temporary) = self.temporary.as_ref() {
                replace_file(temporary, &self.path)?;
                self.temporary = None;
            }
            self.expected_file = None;
        }
        Ok(PublishedProjectMappingBatch {
            path: self.path.clone(),
            parent: self.parent.clone(),
            durability_required,
        })
    }
}

impl Drop for PreparedProjectMappingBatch {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            let _ = fs::remove_file(temporary);
        }
    }
}

impl PublishedProjectMappingBatch {
    pub(crate) fn finish(self) -> io::Result<()> {
        if self.durability_required {
            validate_published_private_file(&self.path, "project mapping file")?;
            sync_directory(&self.parent)?;
        }
        Ok(())
    }
}

impl Default for ProjectMappingStore {
    fn default() -> Self {
        Self::discover()
    }
}

impl ProjectMappingStore {
    pub fn discover() -> Self {
        Self {
            path: default_project_mapping_path(),
        }
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn load(&self) -> io::Result<ProjectMappings> {
        let path = self.required_path()?;
        let parent = mapping_parent(path);
        let _lock = open_locked_lock_file(parent, LockMode::Shared)?;
        read_mappings(path)
    }

    pub fn load_or_create(&self) -> io::Result<ProjectMappings> {
        let path = self.required_path()?;
        let parent = mapping_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;
        load_or_create_locked(path)
    }

    /// Irreversibly removes every derived observation owned by one purged
    /// source while preserving all other physical and logical mappings.
    /// Repeating the operation after a crash is idempotent.
    pub(crate) fn purge_source_observations(&self, source_id: &NodeId) -> io::Result<usize> {
        let Some(path) = self.path.as_deref() else {
            // No durable mapping namespace can have been created when the
            // platform exposes no configuration directory.
            return Ok(0);
        };
        let parent = mapping_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;
        let mut mappings = load_or_create_locked(path)?;
        let before = mappings.instances.len();
        mappings.instances.retain(|instance| {
            instance
                .observations
                .first()
                .is_some_and(|observation| observation.identity.source_id() != source_id)
        });
        let removed = before.saturating_sub(mappings.instances.len());
        if removed > 0 {
            mappings.remove_empty_logical_projects();
            mappings.advance_revision()?;
            canonicalize(&mut mappings);
            mappings.validate()?;
            write_mappings_atomically(path, &serialize_mappings(&mappings)?)?;
        }
        Ok(removed)
    }

    /// Resolves one observation, assigning a fresh physical instance if this
    /// exact `(source, observed key)` has never been recorded. Metadata changes
    /// refresh suggestions only and never alter the assigned instance.
    pub fn resolve_or_create(
        &self,
        expected_revision: u64,
        observation: ProjectObservation,
    ) -> io::Result<ObservationResolution> {
        self.mutate(expected_revision, |mappings| {
            let identity = observation.identity.clone();
            if let Some((instance_index, observation_index)) =
                find_observation_indices(mappings, &identity)
            {
                let instance_id = mappings.instances[instance_index].instance_id.clone();
                let changed = mappings.instances[instance_index].observations[observation_index]
                    .refresh_from(observation)?;
                return Ok(((instance_id, false), changed));
            }

            if mappings.instances.len() >= MAX_INSTANCES
                || total_observation_count(mappings) >= MAX_TOTAL_OBSERVATIONS
            {
                return Err(invalid_mapping("project mapping capacity is exhausted"));
            }
            let instance_id = generate_unique_instance_id(mappings)?;
            mappings.instances.push(ProjectInstanceMapping {
                instance_id: instance_id.clone(),
                observations: vec![StoredObservation::from_observation(observation)?],
                logical_project_id: None,
            });
            canonicalize(mappings);
            Ok(((instance_id, true), true))
        })
        .map(|(mappings, (instance_id, created))| ObservationResolution {
            mappings,
            instance_id,
            created,
        })
    }

    /// Resolves and refreshes a complete descriptor batch under one lock and
    /// one expected-revision check.
    ///
    /// No observation is published unless every item can be represented. This
    /// is the center-side ingest primitive: a remote page can register all of
    /// its source-scoped descriptors without rewriting the mapping file once
    /// per descriptor. Duplicate identities are rejected rather than letting
    /// input order choose which metadata wins.
    pub fn resolve_or_create_batch(
        &self,
        expected_revision: u64,
        observations: Vec<ProjectObservation>,
    ) -> io::Result<ObservationBatchResolution> {
        validate_observation_batch(&observations)?;
        self.mutate(expected_revision, move |mappings| {
            resolve_observation_batch(mappings, observations)
        })
        .map(
            |(mappings, (instance_ids, created_count))| ObservationBatchResolution {
                mappings,
                instance_ids,
                created_count,
            },
        )
    }

    /// Prepares one descriptor batch without publishing it. The blocking
    /// mapping lock and every expensive byte-producing operation are completed
    /// here, before a remotes config fence is acquired. The mapping lock is
    /// released before this method returns; publication later uses a
    /// nonblocking file-identity CAS so it cannot invert the config-to-mapping lock
    /// order used by source lifecycle cleanup.
    pub(crate) fn prepare_resolve_or_create_batch(
        &self,
        observations: Vec<ProjectObservation>,
    ) -> io::Result<PreparedProjectMappingBatch> {
        validate_observation_batch(&observations)?;
        let needs_identity_fence = !observations.is_empty();
        let path = self.required_path()?.to_path_buf();
        let parent = mapping_parent(&path).to_path_buf();
        create_private_directory(&parent)?;
        let lock = open_locked_lock_file(&parent, LockMode::Exclusive)?;
        let mut mappings = load_or_create_locked(&path)?;
        let (_, changed) = resolve_observation_batch(&mut mappings, observations)?;
        if !changed {
            let expected_file = if needs_identity_fence {
                let file = open_mapping_file(&path)?;
                ensure_expected_mapping_file(&path, &file)?;
                Some(file)
            } else {
                None
            };
            drop(lock);
            return Ok(PreparedProjectMappingBatch {
                path,
                parent,
                temporary: None,
                expected_file,
            });
        }

        mappings.advance_revision()?;
        canonicalize(&mut mappings);
        mappings.validate()?;
        let contents = serialize_mappings(&mappings)?;
        let temporary = write_prepared_mappings(&path, &contents)?;
        let expected_file = match open_mapping_file(&path).and_then(|file| {
            ensure_expected_mapping_file(&path, &file)?;
            Ok(file)
        }) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        drop(lock);
        Ok(PreparedProjectMappingBatch {
            path,
            parent,
            temporary: Some(temporary),
            expected_file: Some(expected_file),
        })
    }

    /// Explicitly adds a previously unseen observation as an alias of an
    /// existing physical instance. Physical aliases are always source-local;
    /// cross-source aggregation requires a logical project merge instead. Use
    /// [`Self::move_observation_alias`] when discovery already assigned the
    /// observation its own instance.
    pub fn bind_alias(
        &self,
        expected_revision: u64,
        instance_id: &ProjectInstanceId,
        observation: ProjectObservation,
    ) -> io::Result<ProjectMappings> {
        self.mutate(expected_revision, |mappings| {
            let target = find_instance_index(mappings, instance_id)?;
            let target_source_id = mappings.instances[target]
                .observations
                .first()
                .expect("validated project instances are nonempty")
                .identity
                .source_id();
            if target_source_id != observation.identity.source_id() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a physical project alias must belong to the same source",
                ));
            }
            if let Some((owner, observation_index)) =
                find_observation_indices(mappings, &observation.identity)
            {
                if owner == target {
                    let changed = mappings.instances[target].observations[observation_index]
                        .refresh_from(observation)?;
                    return Ok(((), changed));
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "source project observation already belongs to another instance; use move_observation_alias",
                ));
            }
            if mappings.instances[target].observations.len() >= MAX_OBSERVATIONS_PER_INSTANCE
                || total_observation_count(mappings) >= MAX_TOTAL_OBSERVATIONS
            {
                return Err(invalid_mapping("project observation capacity is exhausted"));
            }
            mappings.instances[target]
                .observations
                .push(StoredObservation::from_observation(observation)?);
            canonicalize(mappings);
            Ok(((), true))
        })
        .map(|(mappings, _)| mappings)
    }

    /// Explicitly moves one already-recorded observation into another physical
    /// instance on the same source. The target instance's logical membership
    /// wins, but two different explicit logical memberships must first be
    /// reconciled by the caller instead of being silently rewritten.
    pub fn move_observation_alias(
        &self,
        expected_revision: u64,
        identity: &SourceObservedProject,
        target_instance_id: &ProjectInstanceId,
    ) -> io::Result<ProjectMappings> {
        self.mutate(expected_revision, |mappings| {
            let (owner_index, observation_index) = find_observation_indices(mappings, identity)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "source project observation is not mapped",
                    )
                })?;
            let target_index = find_instance_index(mappings, target_instance_id)?;
            if owner_index == target_index {
                return Ok(((), false));
            }

            let target_source_id = mappings.instances[target_index]
                .observations
                .first()
                .expect("validated project instances are nonempty")
                .identity
                .source_id();
            if target_source_id != identity.source_id() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a physical project alias must belong to the same source",
                ));
            }
            if mappings.instances[target_index].observations.len()
                >= MAX_OBSERVATIONS_PER_INSTANCE
            {
                return Err(invalid_mapping("project observation capacity is exhausted"));
            }
            let owner_logical = mappings.instances[owner_index].logical_project_id.as_ref();
            let target_logical = mappings.instances[target_index].logical_project_id.as_ref();
            if owner_logical.is_some()
                && target_logical.is_some()
                && owner_logical != target_logical
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "project alias instances belong to different logical projects; split their logical mappings before moving the alias",
                ));
            }

            let observation = mappings.instances[owner_index]
                .observations
                .remove(observation_index);
            if mappings.instances[owner_index].observations.is_empty() {
                mappings.instances.remove(owner_index);
            }
            let target_index = find_instance_index(mappings, target_instance_id)?;
            mappings.instances[target_index].observations.push(observation);
            mappings.remove_empty_logical_projects();
            canonicalize(mappings);
            Ok(((), true))
        })
        .map(|(mappings, _)| mappings)
    }

    /// Moves one alias into a new independent physical instance. This is the
    /// minimal inverse of `bind_alias`; the final alias of an instance cannot
    /// be split because it is already independent.
    pub fn split_observation(
        &self,
        expected_revision: u64,
        identity: &SourceObservedProject,
    ) -> io::Result<ObservationResolution> {
        self.mutate(expected_revision, |mappings| {
            let (instance_index, observation_index) = find_observation_indices(mappings, identity)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "source project observation is not mapped",
                    )
                })?;
            if mappings.instances[instance_index].observations.len() == 1 {
                let instance_id = mappings.instances[instance_index].instance_id.clone();
                return Ok(((instance_id, false), false));
            }
            if mappings.instances.len() >= MAX_INSTANCES {
                return Err(invalid_mapping("project instance capacity is exhausted"));
            }
            let observation = mappings.instances[instance_index]
                .observations
                .remove(observation_index);
            let instance_id = generate_unique_instance_id(mappings)?;
            mappings.instances.push(ProjectInstanceMapping {
                instance_id: instance_id.clone(),
                observations: vec![observation],
                logical_project_id: None,
            });
            canonicalize(mappings);
            Ok(((instance_id, true), true))
        })
        .map(|(mappings, (instance_id, created))| ObservationResolution {
            mappings,
            instance_id,
            created,
        })
    }

    /// Explicitly maps one or more physical instances to a logical project.
    /// `logical_project_id=None` creates a new logical project and requires a
    /// display label. Supplying an existing ID never renames it.
    pub fn merge_instances(
        &self,
        expected_revision: u64,
        logical_project_id: Option<&LogicalProjectId>,
        new_display_label: Option<ProjectDisplayLabel>,
        instance_ids: &[ProjectInstanceId],
    ) -> io::Result<LogicalMergeResult> {
        if instance_ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one project instance is required",
            ));
        }
        let logical_project_id = logical_project_id.cloned();
        self.mutate(expected_revision, move |mappings| {
            let unique = instance_ids.iter().cloned().collect::<BTreeSet<_>>();
            if unique.len() != instance_ids.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "project instance merge contains duplicates",
                ));
            }
            let indexes = unique
                .iter()
                .map(|id| find_instance_index(mappings, id))
                .collect::<io::Result<Vec<_>>>()?;

            let (target, created) = match logical_project_id {
                Some(id) => {
                    if new_display_label.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "renaming an existing logical project requires rename_logical_project",
                        ));
                    }
                    if mappings.logical_project(&id).is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "logical project is not mapped",
                        ));
                    }
                    (id, false)
                }
                None => {
                    if mappings.logical_projects.len() >= MAX_LOGICAL_PROJECTS {
                        return Err(invalid_mapping("logical project capacity is exhausted"));
                    }
                    let label = new_display_label.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a new logical project requires a display label",
                        )
                    })?;
                    let id = generate_unique_logical_project_id(mappings)?;
                    mappings.logical_projects.push(LogicalProjectMapping {
                        logical_project_id: id.clone(),
                        display_label: label,
                    });
                    (id, true)
                }
            };

            let mut changed = created;
            for index in indexes {
                changed |= mappings.instances[index].logical_project_id.as_ref() != Some(&target);
                mappings.instances[index].logical_project_id = Some(target.clone());
            }
            mappings.remove_empty_logical_projects();
            canonicalize(mappings);
            Ok(((target, created), changed))
        })
        .map(
            |(mappings, (logical_project_id, created))| LogicalMergeResult {
                mappings,
                logical_project_id,
                created,
            },
        )
    }

    /// Removes explicit logical membership for selected instances. Empty
    /// logical project records are pruned, so the operation is a full inverse
    /// for a newly created merge.
    pub fn split_instances(
        &self,
        expected_revision: u64,
        instance_ids: &[ProjectInstanceId],
    ) -> io::Result<ProjectMappings> {
        if instance_ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one project instance is required",
            ));
        }
        self.mutate(expected_revision, |mappings| {
            let unique = instance_ids.iter().cloned().collect::<BTreeSet<_>>();
            if unique.len() != instance_ids.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "project instance split contains duplicates",
                ));
            }
            let indexes = unique
                .iter()
                .map(|id| find_instance_index(mappings, id))
                .collect::<io::Result<Vec<_>>>()?;
            let mut changed = false;
            for index in indexes {
                changed |= mappings.instances[index]
                    .logical_project_id
                    .take()
                    .is_some();
            }
            mappings.remove_empty_logical_projects();
            canonicalize(mappings);
            Ok(((), changed))
        })
        .map(|(mappings, _)| mappings)
    }

    pub fn rename_logical_project(
        &self,
        expected_revision: u64,
        logical_project_id: &LogicalProjectId,
        display_label: ProjectDisplayLabel,
    ) -> io::Result<ProjectMappings> {
        self.mutate(expected_revision, |mappings| {
            let index = mappings
                .logical_projects
                .binary_search_by(|entry| entry.logical_project_id.cmp(logical_project_id))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::NotFound, "logical project is not mapped")
                })?;
            let changed = mappings.logical_projects[index].display_label != display_label;
            mappings.logical_projects[index].display_label = display_label;
            Ok(((), changed))
        })
        .map(|(mappings, _)| mappings)
    }

    fn mutate<R>(
        &self,
        expected_revision: u64,
        operation: impl FnOnce(&mut ProjectMappings) -> io::Result<(R, bool)>,
    ) -> io::Result<(ProjectMappings, R)> {
        let path = self.required_path()?;
        let parent = mapping_parent(path);
        create_private_directory(parent)?;
        let _lock = open_locked_lock_file(parent, LockMode::Exclusive)?;
        let mut mappings = load_or_create_locked(path)?;
        ensure_expected_revision(expected_revision, mappings.revision)?;
        let (result, changed) = operation(&mut mappings)?;
        if changed {
            mappings.advance_revision()?;
            canonicalize(&mut mappings);
            mappings.validate()?;
            write_mappings_atomically(path, &serialize_mappings(&mappings)?)?;
        }
        Ok((mappings, result))
    }

    fn required_path(&self) -> io::Result<&Path> {
        self.path.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no user-level configuration directory is available",
            )
        })
    }
}

fn find_instance_index(
    mappings: &ProjectMappings,
    instance_id: &ProjectInstanceId,
) -> io::Result<usize> {
    mappings
        .instances
        .binary_search_by(|entry| entry.instance_id.cmp(instance_id))
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "project instance is not mapped"))
}

fn find_observation_indices(
    mappings: &ProjectMappings,
    identity: &SourceObservedProject,
) -> Option<(usize, usize)> {
    mappings
        .instances
        .iter()
        .enumerate()
        .find_map(|(instance_index, instance)| {
            instance
                .observations
                .binary_search_by(|entry| entry.identity.cmp(identity))
                .ok()
                .map(|observation_index| (instance_index, observation_index))
        })
}

fn validate_observation_batch(observations: &[ProjectObservation]) -> io::Result<()> {
    let mut identities = BTreeSet::new();
    if observations
        .iter()
        .any(|observation| !identities.insert(observation.identity.clone()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "project observation batch contains duplicate identities",
        ));
    }
    Ok(())
}

/// Resolves one already-validated batch in linear expected time. The previous
/// implementation repeatedly scanned every existing instance and recounted all
/// observations for each untrusted descriptor, turning a legal maximum-sized
/// page into quadratic work.
fn resolve_observation_batch(
    mappings: &mut ProjectMappings,
    observations: Vec<ProjectObservation>,
) -> io::Result<((Vec<ProjectInstanceId>, usize), bool)> {
    let mut observation_indices = HashMap::with_capacity(total_observation_count(mappings));
    for (instance_index, instance) in mappings.instances.iter().enumerate() {
        for (observation_index, observation) in instance.observations.iter().enumerate() {
            observation_indices.insert(
                observation.identity.clone(),
                (instance_index, observation_index),
            );
        }
    }
    let mut total_observations = observation_indices.len();
    let mut instance_ids = Vec::with_capacity(observations.len());
    let mut created_count = 0_usize;
    let mut changed = false;
    let mut allocated_ids = mappings
        .instances
        .iter()
        .map(|instance| instance.instance_id.clone())
        .collect::<BTreeSet<_>>();

    for observation in observations {
        let identity = observation.identity.clone();
        if let Some(&(instance_index, observation_index)) = observation_indices.get(&identity) {
            let instance_id = mappings.instances[instance_index].instance_id.clone();
            changed |= mappings.instances[instance_index].observations[observation_index]
                .refresh_from(observation)?;
            instance_ids.push(instance_id);
            continue;
        }

        if mappings.instances.len() >= MAX_INSTANCES || total_observations >= MAX_TOTAL_OBSERVATIONS
        {
            return Err(invalid_mapping("project mapping capacity is exhausted"));
        }
        let instance_id = generate_unique_instance_id_from_set(&mut allocated_ids)?;
        let instance_index = mappings.instances.len();
        mappings.instances.push(ProjectInstanceMapping {
            instance_id: instance_id.clone(),
            observations: vec![StoredObservation::from_observation(observation)?],
            logical_project_id: None,
        });
        observation_indices.insert(identity, (instance_index, 0));
        total_observations = total_observations.saturating_add(1);
        instance_ids.push(instance_id);
        created_count = created_count.saturating_add(1);
        changed = true;
    }
    canonicalize(mappings);
    Ok(((instance_ids, created_count), changed))
}

fn total_observation_count(mappings: &ProjectMappings) -> usize {
    mappings
        .instances
        .iter()
        .map(|instance| instance.observations.len())
        .sum()
}

fn generate_unique_instance_id(mappings: &ProjectMappings) -> io::Result<ProjectInstanceId> {
    let mut allocated = mappings
        .instances
        .iter()
        .map(|instance| instance.instance_id.clone())
        .collect::<BTreeSet<_>>();
    generate_unique_instance_id_from_set(&mut allocated)
}

fn generate_unique_instance_id_from_set(
    allocated: &mut BTreeSet<ProjectInstanceId>,
) -> io::Result<ProjectInstanceId> {
    for _ in 0..8 {
        let candidate = ProjectInstanceId::generate()?;
        if allocated.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(io::Error::other(
        "secure random provider repeatedly returned an existing project instance ID",
    ))
}

fn generate_unique_logical_project_id(mappings: &ProjectMappings) -> io::Result<LogicalProjectId> {
    for _ in 0..8 {
        let candidate = LogicalProjectId::generate()?;
        if mappings.logical_project(&candidate).is_none() {
            return Ok(candidate);
        }
    }
    Err(io::Error::other(
        "secure random provider repeatedly returned an existing logical project ID",
    ))
}

fn canonicalize(mappings: &mut ProjectMappings) {
    for instance in &mut mappings.instances {
        instance
            .observations
            .sort_by(|left, right| left.identity.cmp(&right.identity));
    }
    mappings
        .instances
        .sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    mappings
        .logical_projects
        .sort_by(|left, right| left.logical_project_id.cmp(&right.logical_project_id));
}

fn validate_repository_relative_root(value: Option<&str>) -> io::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value == "." {
        return Ok(());
    }
    if value.is_empty()
        || value.len() > MAX_REPOSITORY_RELATIVE_ROOT_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{2028}'
                        | '\u{2029}'
                )
        })
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid_mapping(
            "repositoryRelativeWorkspaceRoot is not a safe normalized relative path",
        ));
    }
    Ok(())
}

fn validate_git_evidence(value: &RemoteGitRepositoryEvidence) -> io::Result<()> {
    validate_repository_relative_root(value.repository_relative_workspace_root())
}

fn ensure_expected_revision(expected: u64, actual: u64) -> io::Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "stale project mapping revision {expected}; current revision is {actual}; reload before retrying"
        ),
    ))
}

fn serialize_mappings(mappings: &ProjectMappings) -> io::Result<Vec<u8>> {
    mappings.validate()?;
    let mut contents = serde_json::to_vec_pretty(mappings)
        .map_err(|error| invalid_mapping(format!("could not encode project mappings: {error}")))?;
    contents.push(b'\n');
    if contents.len() as u64 > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    Ok(contents)
}

fn deserialize_mappings(contents: &[u8]) -> io::Result<ProjectMappings> {
    let probe = serde_json::from_slice::<VersionProbe>(contents)
        .map_err(|error| invalid_mapping(format!("invalid project mappings: {error}")))?;
    let Some(version) = probe.version else {
        return Err(invalid_mapping("project mapping file is missing version"));
    };
    if version != PROJECT_MAPPING_VERSION {
        return Err(invalid_mapping(format!(
            "unsupported project mapping version {version}; expected {PROJECT_MAPPING_VERSION}"
        )));
    }
    let mappings: ProjectMappings = serde_json::from_slice(contents)
        .map_err(|error| invalid_mapping(format!("invalid project mappings: {error}")))?;
    mappings.validate()?;
    Ok(mappings)
}

#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: Option<u32>,
}

fn invalid_mapping(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn default_project_mapping_path() -> Option<PathBuf> {
    resolve_project_mapping_path(
        nonempty_env(CONFIG_DIRECTORY_ENV).as_deref(),
        nonempty_env("XDG_CONFIG_HOME").as_deref(),
        nonempty_env("HOME").as_deref(),
        nonempty_env("LOCALAPPDATA").as_deref(),
        nonempty_env("USERPROFILE").as_deref(),
        current_platform(),
    )
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Windows,
    Unix,
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn resolve_project_mapping_path(
    override_directory: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
    user_profile: Option<&Path>,
    platform: Platform,
) -> Option<PathBuf> {
    if let Some(directory) = nonempty_path(override_directory) {
        return Some(directory.join(MAPPING_FILE));
    }
    if let Some(directory) = nonempty_path(xdg_config_home) {
        return Some(directory.join(APP_DIRECTORY).join(MAPPING_FILE));
    }
    let directory = match platform {
        Platform::MacOs => nonempty_path(home).map(|path| path.join("Library/Application Support")),
        Platform::Windows => nonempty_path(local_app_data)
            .map(Path::to_path_buf)
            .or_else(|| nonempty_path(user_profile).map(|path| path.join("AppData").join("Local"))),
        Platform::Unix => nonempty_path(home).map(|path| path.join(".config")),
    }?;
    Some(directory.join(APP_DIRECTORY).join(MAPPING_FILE))
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

fn mapping_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn load_or_create_locked(path: &Path) -> io::Result<ProjectMappings> {
    match read_mappings(path) {
        Ok(mappings) => Ok(mappings),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let default = ProjectMappings::default();
            let contents = serialize_mappings(&default)?;
            if create_private_atomically(path, &contents)? {
                Ok(default)
            } else {
                read_mappings(path)
            }
        }
        Err(error) => Err(error),
    }
}

fn read_mappings(path: &Path) -> io::Result<ProjectMappings> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_mapping_file_metadata(&path_metadata, "project mapping path")?;
    let mut file = open_mapping_file(path)?;
    let opened_metadata = file.metadata()?;
    validate_mapping_file_metadata(&opened_metadata, "project mapping path")?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &opened_metadata,
        "project mapping file",
    )?;
    if opened_metadata.len() > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    ensure_private_file(path, &file, &opened_metadata, "project mapping file")?;

    let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_MAPPING_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    deserialize_mappings(&contents)
}

fn open_mapping_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "project mapping path"))
}

fn write_mappings_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    if contents.len() as u64 > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    let parent = mapping_parent(path);
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(MAPPING_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path, "project mapping file")?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_prepared_mappings(path: &Path, contents: &[u8]) -> io::Result<PathBuf> {
    if contents.len() as u64 > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    let parent = mapping_parent(path);
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(MAPPING_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        validate_published_private_file(&temporary, "project mapping prepared file")?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(temporary)
}

fn create_private_atomically(path: &Path, contents: &[u8]) -> io::Result<bool> {
    if contents.len() as u64 > MAX_MAPPING_FILE_BYTES {
        return Err(invalid_mapping("project mapping file is too large"));
    }
    let parent = mapping_parent(path);
    create_private_directory(parent)?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new(MAPPING_FILE));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                validate_published_private_file(path, "project mapping file")?;
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
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
        match options.open(&temporary) {
            Ok(file) => {
                let metadata = file.metadata()?;
                validate_mapping_file_metadata(&metadata, "project mapping temporary file")?;
                ensure_private_file(
                    &temporary,
                    &file,
                    &metadata,
                    "project mapping temporary file",
                )?;
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique project mapping temporary file",
    ))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match validate_private_directory(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
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
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_mapping(
            "project mapping directory must not be a symbolic link or reparse point",
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_mapping(
            "project mapping directory path must be a directory",
        ));
    }
    ensure_private_directory(path, &metadata, "project mapping directory")
}

fn validate_mapping_file_metadata(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_mapping(format!(
            "{subject} must not be a symbolic link or reparse point"
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_mapping(format!("{subject} must be a regular file")));
    }
    Ok(())
}

fn validate_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    validate_mapping_file_metadata(metadata, "project mapping lock")?;
    ensure_private_path(metadata, "project mapping lock")
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_mapping_file_metadata(&path_metadata, subject)?;
    let file = open_mapping_file(path)?;
    let opened_metadata = file.metadata()?;
    validate_mapping_file_metadata(&opened_metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &opened_metadata, subject)?;
    ensure_private_file(path, &file, &opened_metadata, subject)
}

/// Verifies that `expected_file` is still the exact file linked at `path`.
/// Every mapping-store mutation replaces the file atomically while holding the
/// stable lock, so file identity is the publication CAS and does not require
/// reparsing the bounded-but-potentially-large JSON document.
fn ensure_expected_mapping_file(path: &Path, expected_file: &File) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_mapping_file_metadata(&path_metadata, "project mapping path")?;
    let expected_metadata = expected_file.metadata()?;
    validate_mapping_file_metadata(&expected_metadata, "prepared project mapping file")?;
    ensure_private_file(
        path,
        expected_file,
        &expected_metadata,
        "prepared project mapping file",
    )?;
    ensure_opened_file_matches_path(
        path,
        expected_file,
        &path_metadata,
        &expected_metadata,
        "project mapping file",
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "project mapping changed after remote page staging",
            )
        } else {
            error
        }
    })
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_file(path, file, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private project mapping files are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_private_directory(
    _path: &Path,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_directory(
    path: &Path,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_directory(path, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(
    _path: &Path,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private project mapping directories are unsupported on this platform",
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockMode {
    Shared,
    Exclusive,
}

fn open_lock_file(directory: &Path) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_lock_metadata(&metadata)?,
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
        .map_err(|error| map_nofollow_error(error, "project mapping lock"))?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_private_file(&path, &file, &opened_metadata, "project mapping lock")?;
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &opened_metadata,
        "project mapping lock",
    )?;
    Ok(file)
}

fn open_locked_lock_file(directory: &Path, mode: LockMode) -> io::Result<File> {
    let file = open_lock_file(directory)?;
    match mode {
        LockMode::Shared => fs2::FileExt::lock_shared(&file)?,
        LockMode::Exclusive => fs2::FileExt::lock_exclusive(&file)?,
    }
    validate_private_directory(directory)?;
    let path = directory.join(LOCK_FILE);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_private_file(&path, &file, &opened_metadata, "project mapping lock")?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &opened_metadata,
        "project mapping lock",
    )?;
    Ok(file)
}

fn try_open_locked_lock_file(directory: &Path, mode: LockMode) -> io::Result<Option<File>> {
    let file = open_lock_file(directory)?;
    let result = match mode {
        LockMode::Shared => fs2::FileExt::try_lock_shared(&file),
        LockMode::Exclusive => fs2::FileExt::try_lock_exclusive(&file),
    };
    match result {
        Ok(()) => {
            validate_private_directory(directory)?;
            let path = directory.join(LOCK_FILE);
            let path_metadata = fs::symlink_metadata(&path)?;
            validate_lock_metadata(&path_metadata)?;
            let opened_metadata = file.metadata()?;
            validate_lock_metadata(&opened_metadata)?;
            ensure_private_file(&path, &file, &opened_metadata, "project mapping lock")?;
            ensure_opened_file_matches_path(
                &path,
                &file,
                &path_metadata,
                &opened_metadata,
                "project mapping lock",
            )?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
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

#[cfg(any(test, windows))]
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
        return invalid_mapping(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
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
        Err(invalid_mapping(format!(
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
        Err(invalid_mapping(format!(
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
    // SAFETY: the live raw handle remains owned for this call and the API
    // initializes the output structure on success.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported success, so the structure is initialized.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::str::FromStr;
    use std::time::{Duration as StdDuration, Instant};
    use tempfile::tempdir;

    use super::*;

    const NODE_ONE: &str = "node-0123456789abcdef0123456789abcdef";
    const NODE_TWO: &str = "node-fedcba9876543210fedcba9876543210";

    fn observed(hex: char) -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn identity(node: &str, hex: char) -> SourceObservedProject {
        SourceObservedProject::new(node.parse().unwrap(), observed(hex))
    }

    fn observation(node: &str, hex: char, label: &str) -> ProjectObservation {
        ProjectObservation::new(identity(node, hex))
            .with_display_label(Some(label.parse().unwrap()))
    }

    fn store(directory: &tempfile::TempDir) -> ProjectMappingStore {
        ProjectMappingStore::new(directory.path().join("config/project-mappings.json"))
    }

    #[test]
    fn same_key_is_stable_on_one_source_and_separate_across_sources() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let initial = store.load_or_create().unwrap();
        let first = store
            .resolve_or_create(initial.revision(), observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let repeated = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_ONE, 'a', "renamed alpha"),
            )
            .unwrap();
        let remote = store
            .resolve_or_create(
                repeated.mappings().revision(),
                observation(NODE_TWO, 'a', "alpha"),
            )
            .unwrap();

        assert_eq!(first.instance_id(), repeated.instance_id());
        assert!(!repeated.created());
        assert_ne!(first.instance_id(), remote.instance_id());
        assert_eq!(remote.mappings().instances().len(), 2);
        assert_eq!(remote.mappings().projection().len(), 2);
    }

    #[test]
    fn source_purge_preserves_other_source_mappings_across_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config/project-mappings.json");
        let store = ProjectMappingStore::new(path.clone());
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "one"))
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "two"),
            )
            .unwrap();
        assert_eq!(
            store
                .purge_source_observations(&NODE_ONE.parse().unwrap())
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .purge_source_observations(&NODE_ONE.parse().unwrap())
                .unwrap(),
            0,
            "purge is idempotent"
        );

        let restarted = ProjectMappingStore::new(path);
        let mappings = restarted.load().unwrap();
        assert_eq!(mappings.instances().len(), 1);
        assert_eq!(
            mappings.instances()[0]
                .observations()
                .next()
                .unwrap()
                .source_id()
                .as_str(),
            NODE_TWO
        );
        assert_eq!(mappings.instances()[0].instance_id(), second.instance_id());
    }

    #[test]
    fn batch_resolution_is_atomic_cas_bound_and_refreshes_metadata_once() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let initial = store.load_or_create().unwrap();
        let created = store
            .resolve_or_create_batch(
                initial.revision(),
                vec![
                    observation(NODE_ONE, 'a', "alpha"),
                    observation(NODE_ONE, 'b', "beta"),
                ],
            )
            .unwrap();
        assert_eq!(created.created_count(), 2);
        assert_eq!(created.instance_ids().len(), 2);
        assert_eq!(created.mappings().revision(), initial.revision() + 1);

        let refreshed = store
            .resolve_or_create_batch(
                created.mappings().revision(),
                vec![
                    observation(NODE_ONE, 'a', "renamed alpha"),
                    observation(NODE_ONE, 'b', "renamed beta"),
                ],
            )
            .unwrap();
        assert_eq!(refreshed.created_count(), 0);
        assert_eq!(refreshed.instance_ids(), created.instance_ids());
        assert_eq!(
            refreshed.mappings().revision(),
            created.mappings().revision() + 1
        );
        assert_eq!(
            refreshed
                .mappings()
                .projection()
                .resolve(&NODE_ONE.parse().unwrap(), &observed('a'))
                .unwrap()
                .display_label()
                .unwrap()
                .as_str(),
            "renamed alpha"
        );

        let before_stale = store.load().unwrap();
        let stale = store
            .resolve_or_create_batch(
                created.mappings().revision(),
                vec![observation(NODE_ONE, 'c', "gamma")],
            )
            .unwrap_err();
        assert_eq!(stale.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(store.load().unwrap(), before_stale);

        let invalid = ProjectObservation {
            identity: identity(NODE_ONE, 'd'),
            display_label: Some("invalid".parse().unwrap()),
            git_evidence: RemoteGitRepositoryEvidence::Repository {
                fingerprint: None,
                repository_relative_workspace_root: "../workspace".to_string(),
            },
        };
        let error = store
            .resolve_or_create_batch(
                before_stale.revision(),
                vec![observation(NODE_ONE, 'c', "gamma"), invalid],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(store.load().unwrap(), before_stale);
    }

    #[test]
    fn prepared_batch_publication_is_nonblocking_when_mapping_is_busy() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        store.load_or_create().unwrap();
        let prepared = store
            .prepare_resolve_or_create_batch(vec![observation(NODE_ONE, 'a', "alpha")])
            .unwrap();
        let config = directory.path().join("config");
        let lock = open_locked_lock_file(&config, LockMode::Exclusive).unwrap();

        let started = Instant::now();
        let error = prepared.publish().unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(
            elapsed < StdDuration::from_millis(250),
            "mapping try-lock unexpectedly waited for {elapsed:?}"
        );
        drop(lock);
        assert!(store.load().unwrap().projection().is_empty());
    }

    #[test]
    fn prepared_batch_file_identity_cas_rejects_a_concurrent_mapping_change() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        store.load_or_create().unwrap();
        let prepared = store
            .prepare_resolve_or_create_batch(vec![observation(NODE_ONE, 'a', "staged")])
            .unwrap();
        let concurrent = store
            .resolve_or_create(0, observation(NODE_ONE, 'b', "concurrent"))
            .unwrap();

        let error = prepared.publish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let projection = store.load().unwrap().projection();
        assert_eq!(projection.len(), 1);
        assert!(
            projection
                .resolve(&NODE_ONE.parse().unwrap(), &observed('a'))
                .is_none()
        );
        assert_eq!(concurrent.mappings().revision(), 1);
    }

    #[test]
    fn unchanged_prepared_batch_still_rejects_a_concurrent_mapping_change() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let seeded = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "stable"))
            .unwrap();
        let prepared = store
            .prepare_resolve_or_create_batch(vec![observation(NODE_ONE, 'a', "stable")])
            .unwrap();
        store
            .resolve_or_create(
                seeded.mappings().revision(),
                observation(NODE_ONE, 'b', "concurrent"),
            )
            .unwrap();

        let error = prepared.publish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(store.load().unwrap().projection().len(), 2);
    }

    #[test]
    fn unchanged_nonempty_batch_retries_publication_validation_and_directory_sync() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "stable"))
            .unwrap();
        let published = store
            .prepare_resolve_or_create_batch(vec![observation(NODE_ONE, 'a', "stable")])
            .unwrap()
            .publish()
            .unwrap();

        fs::remove_file(store.path().unwrap()).unwrap();
        assert_eq!(
            published.finish().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn batch_rejects_duplicate_identities_without_publishing_a_partial_refresh() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let initial = store
            .resolve_or_create_batch(0, vec![observation(NODE_ONE, 'a', "alpha")])
            .unwrap();
        let before = initial.mappings().clone();
        let error = store
            .resolve_or_create_batch(
                before.revision(),
                vec![
                    observation(NODE_ONE, 'a', "first refresh"),
                    observation(NODE_ONE, 'a', "second refresh"),
                ],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(store.load().unwrap(), before);
    }

    #[test]
    fn explicit_merge_split_and_rename_are_reversible() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "alpha"),
            )
            .unwrap();
        let first_id = first.instance_id().clone();
        let second_id = second.instance_id().clone();
        let merged = store
            .merge_instances(
                second.mappings().revision(),
                None,
                Some("unified alpha".parse().unwrap()),
                &[first_id.clone(), second_id.clone()],
            )
            .unwrap();
        let logical = merged.logical_project_id().clone();
        assert!(merged.created());

        let renamed = store
            .rename_logical_project(
                merged.mappings().revision(),
                &logical,
                "renamed unified".parse().unwrap(),
            )
            .unwrap();
        assert_eq!(
            renamed
                .logical_project(&logical)
                .unwrap()
                .display_label()
                .as_str(),
            "renamed unified"
        );
        let split = store
            .split_instances(renamed.revision(), std::slice::from_ref(&second_id))
            .unwrap();
        assert_eq!(
            split.instance(&first_id).unwrap().logical_project_id(),
            Some(&logical)
        );
        assert!(
            split
                .instance(&second_id)
                .unwrap()
                .logical_project_id()
                .is_none()
        );
        let fully_split = store
            .split_instances(split.revision(), &[first_id])
            .unwrap();
        assert!(fully_split.logical_projects().is_empty());
    }

    #[test]
    fn aliases_can_be_explicitly_bound_and_split() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let instance = first.instance_id().clone();
        let discovered_alias = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_ONE, 'b', "alpha moved"),
            )
            .unwrap();
        assert_ne!(discovered_alias.instance_id(), &instance);
        assert_eq!(discovered_alias.mappings().instances().len(), 2);

        let aliased = store
            .move_observation_alias(
                discovered_alias.mappings().revision(),
                &identity(NODE_ONE, 'b'),
                &instance,
            )
            .unwrap();
        assert_eq!(aliased.instances().len(), 1);
        assert_eq!(aliased.instance(&instance).unwrap().observations().len(), 2);
        let projection = aliased.projection();
        assert_eq!(
            projection
                .resolve(&NODE_ONE.parse().unwrap(), &observed('a'))
                .unwrap()
                .display_label(),
            projection
                .resolve(&NODE_ONE.parse().unwrap(), &observed('b'))
                .unwrap()
                .display_label()
        );

        let split = store
            .split_observation(aliased.revision(), &identity(NODE_ONE, 'b'))
            .unwrap();
        assert!(split.created());
        assert_ne!(split.instance_id(), &instance);
        assert_eq!(split.mappings().instances().len(), 2);
    }

    #[test]
    fn physical_aliases_cannot_cross_source_boundaries() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let error = store
            .bind_alias(
                first.mappings().revision(),
                first.instance_id(),
                observation(NODE_TWO, 'b', "alpha remote"),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let remote = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "alpha remote"),
            )
            .unwrap();
        let error = store
            .move_observation_alias(
                remote.mappings().revision(),
                &identity(NODE_TWO, 'b'),
                first.instance_id(),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(store.load().unwrap(), *remote.mappings());
    }

    #[test]
    fn alias_move_rejects_conflicting_explicit_logical_memberships() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_ONE, 'b', "beta"),
            )
            .unwrap();
        let first_id = first.instance_id().clone();
        let second_id = second.instance_id().clone();
        let first_logical = store
            .merge_instances(
                second.mappings().revision(),
                None,
                Some("logical alpha".parse().unwrap()),
                std::slice::from_ref(&first_id),
            )
            .unwrap();
        let second_logical = store
            .merge_instances(
                first_logical.mappings().revision(),
                None,
                Some("logical beta".parse().unwrap()),
                std::slice::from_ref(&second_id),
            )
            .unwrap();

        let before = second_logical.mappings().clone();
        let error = store
            .move_observation_alias(before.revision(), &identity(NODE_ONE, 'b'), &first_id)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(store.load().unwrap(), before);
    }

    #[test]
    fn evidence_only_produces_suggestions_and_never_auto_merges() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let fingerprint = format!("git-sha256-v1-{}", "c".repeat(64))
            .parse::<GitRepositoryFingerprint>()
            .unwrap();
        let first_observation = observation(NODE_ONE, 'a', "alpha")
            .with_git_evidence(Some(fingerprint.clone()), Some("crates/app".to_string()))
            .unwrap();
        let second_observation = observation(NODE_TWO, 'b', "alpha")
            .with_git_evidence(Some(fingerprint), Some("crates/app".to_string()))
            .unwrap();
        let first = store.resolve_or_create(0, first_observation).unwrap();
        let second = store
            .resolve_or_create(first.mappings().revision(), second_observation)
            .unwrap();

        assert_ne!(first.instance_id(), second.instance_id());
        assert_eq!(second.mappings().logical_projects().len(), 0);
        let suggestions = second.mappings().merge_suggestions();
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions.iter().any(|suggestion| matches!(
            suggestion.reason(),
            ProjectMergeSuggestionReason::MatchingGit { .. }
        )));
    }

    #[test]
    fn git_refresh_distinguishes_unavailable_no_origin_and_confirmed_non_repository() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first_fingerprint = format!("git-sha256-v1-{}", "1".repeat(64))
            .parse::<GitRepositoryFingerprint>()
            .unwrap();
        let second_fingerprint = format!("git-sha256-v1-{}", "2".repeat(64))
            .parse::<GitRepositoryFingerprint>()
            .unwrap();
        let first = store
            .resolve_or_create(
                0,
                observation(NODE_ONE, 'a', "alpha")
                    .with_git_evidence(Some(first_fingerprint.clone()), Some(".".to_owned()))
                    .unwrap(),
            )
            .unwrap();

        // An explicitly unavailable bounded probe preserves prior evidence.
        let unavailable = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_ONE, 'a', "alpha"),
            )
            .unwrap();
        let encoded = serde_json::to_string(unavailable.mappings()).unwrap();
        assert!(encoded.contains(first_fingerprint.as_str()));

        let changed = store
            .resolve_or_create(
                unavailable.mappings().revision(),
                observation(NODE_ONE, 'a', "alpha")
                    .with_git_evidence(Some(second_fingerprint.clone()), Some(".".to_owned()))
                    .unwrap(),
            )
            .unwrap();
        let encoded = serde_json::to_string(changed.mappings()).unwrap();
        assert!(encoded.contains(second_fingerprint.as_str()));
        assert!(!encoded.contains(first_fingerprint.as_str()));

        // A verified repository with no fingerprint clears the old origin but
        // retains its repository-relative root.
        let no_origin = store
            .resolve_or_create(
                changed.mappings().revision(),
                observation(NODE_ONE, 'a', "alpha")
                    .with_git_evidence(None, Some(".".to_owned()))
                    .unwrap(),
            )
            .unwrap();
        let encoded = serde_json::to_string(no_origin.mappings()).unwrap();
        assert!(!encoded.contains("git-sha256-v1-"));
        assert!(encoded.contains("repositoryRelativeWorkspaceRoot"));

        // Confirmed non-repository is also authoritative, but clears both
        // root and fingerprint instead of being mistaken for unavailability.
        let non_repository = store
            .resolve_or_create(
                no_origin.mappings().revision(),
                observation(NODE_ONE, 'a', "alpha")
                    .with_git_evidence_state(RemoteGitRepositoryEvidence::ConfirmedNonRepository)
                    .unwrap(),
            )
            .unwrap();
        let encoded = serde_json::to_string(non_repository.mappings()).unwrap();
        assert!(!encoded.contains("git-sha256-v1-"));
        assert!(!encoded.contains("repositoryRelativeWorkspaceRoot"));
        assert!(encoded.contains("confirmed_non_repository"));
    }

    #[test]
    fn unavailable_git_and_matching_labels_do_not_suggest_merges() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "same"))
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "same"),
            )
            .unwrap();

        assert!(second.mappings().merge_suggestions().is_empty());
    }

    #[test]
    fn git_suggestions_keep_distinct_monorepo_workspace_roots_separate() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let fingerprint = format!("git-sha256-v1-{}", "e".repeat(64))
            .parse::<GitRepositoryFingerprint>()
            .unwrap();
        let first = store
            .resolve_or_create(
                0,
                observation(NODE_ONE, 'a', "frontend")
                    .with_git_evidence(Some(fingerprint.clone()), Some("apps/frontend".to_string()))
                    .unwrap(),
            )
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "backend")
                    .with_git_evidence(Some(fingerprint), Some("apps/backend".to_string()))
                    .unwrap(),
            )
            .unwrap();

        assert!(
            second
                .mappings()
                .merge_suggestions()
                .iter()
                .all(|suggestion| !matches!(
                    suggestion.reason(),
                    ProjectMergeSuggestionReason::MatchingGit { .. }
                ))
        );
    }

    #[test]
    fn state_survives_restart_and_projection_is_pure() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config/project-mappings.json");
        let store = ProjectMappingStore::new(path.clone());
        let resolved = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let instance = resolved.instance_id().clone();
        drop(store);

        let reopened = ProjectMappingStore::new(path);
        let loaded = reopened.load().unwrap();
        assert_eq!(loaded, *resolved.mappings());
        let projection = loaded.projection();
        let projected = projection
            .resolve(&NODE_ONE.parse().unwrap(), &observed('a'))
            .unwrap();
        assert_eq!(projected.instance_id(), &instance);
        assert_eq!(
            projected.aggregate_id(),
            &ProjectAggregateId::Instance(instance)
        );
    }

    #[test]
    fn stale_revision_is_rejected_without_mutating_disk() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let first = store
            .resolve_or_create(0, observation(NODE_ONE, 'a', "alpha"))
            .unwrap();
        let before = store.load().unwrap();
        let error = store
            .resolve_or_create(0, observation(NODE_TWO, 'b', "beta"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(store.load().unwrap(), before);
        assert_eq!(first.mappings(), &before);
    }

    #[test]
    fn malformed_future_unknown_and_duplicate_state_fail_closed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config/project-mappings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        }

        for value in [
            json!({"version": 99, "revision": 0, "instances": [], "logicalProjects": []}),
            json!({"version": 1, "revision": 0, "instances": [], "logicalProjects": [], "future": true}),
            json!({
                "version": 1,
                "revision": 1,
                "instances": [
                    {
                        "instanceId": format!("project-instance-{}", "1".repeat(32)),
                        "observations": [{
                            "identity": {
                                "sourceId": NODE_ONE,
                                "observedProjectKey": observed('a')
                            }
                        }]
                    },
                    {
                        "instanceId": format!("project-instance-{}", "2".repeat(32)),
                        "observations": [{
                            "identity": {
                                "sourceId": NODE_ONE,
                                "observedProjectKey": observed('a')
                            }
                        }]
                    }
                ],
                "logicalProjects": []
            }),
            json!({
                "version": 1,
                "revision": 1,
                "instances": [{
                    "instanceId": format!("project-instance-{}", "3".repeat(32)),
                    "observations": [
                        {
                            "identity": {
                                "sourceId": NODE_ONE,
                                "observedProjectKey": observed('a')
                            }
                        },
                        {
                            "identity": {
                                "sourceId": NODE_TWO,
                                "observedProjectKey": observed('b')
                            }
                        }
                    ]
                }],
                "logicalProjects": []
            }),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(&path, bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let error = ProjectMappingStore::new(path.clone()).load().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[cfg(unix)]
    #[test]
    fn mapping_and_lock_symlinks_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let config = directory.path().join("config");
        fs::create_dir(&config).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"{}").unwrap();
        let mapping = config.join(MAPPING_FILE);
        symlink(&target, &mapping).unwrap();
        assert!(ProjectMappingStore::new(mapping.clone()).load().is_err());

        fs::remove_file(mapping).unwrap();
        let store = ProjectMappingStore::new(config.join(MAPPING_FILE));
        store.load_or_create().unwrap();
        let lock = config.join(LOCK_FILE);
        fs::remove_file(&lock).unwrap();
        symlink(&target, &lock).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn platform_paths_match_remotes_config_layout() {
        assert_eq!(
            resolve_project_mapping_path(
                Some(Path::new("override")),
                Some(Path::new("xdg")),
                Some(Path::new("home")),
                Some(Path::new("local")),
                Some(Path::new("profile")),
                Platform::Unix,
            ),
            Some(PathBuf::from("override").join(MAPPING_FILE))
        );
        assert_eq!(
            resolve_project_mapping_path(
                None,
                None,
                Some(Path::new("home")),
                None,
                None,
                Platform::MacOs,
            ),
            Some(
                PathBuf::from("home")
                    .join("Library/Application Support")
                    .join(APP_DIRECTORY)
                    .join(MAPPING_FILE)
            )
        );
        assert_eq!(stable_lock_share_mode(), 0x0000_0001 | 0x0000_0002);
    }

    #[test]
    fn malformed_repository_relative_roots_are_rejected() {
        assert!(
            ProjectObservation::new(identity(NODE_ONE, 'a'))
                .with_git_evidence(
                    Some(
                        GitRepositoryFingerprint::from_str(&format!(
                            "git-sha256-v1-{}",
                            "d".repeat(64)
                        ))
                        .unwrap()
                    ),
                    Some(".".to_owned())
                )
                .is_ok()
        );
        for invalid in ["", "/root", "../root", "a//b", "a/./b", "a\\..\\b"] {
            assert!(
                ProjectObservation::new(identity(NODE_ONE, 'a'))
                    .with_git_evidence(
                        Some(
                            GitRepositoryFingerprint::from_str(&format!(
                                "git-sha256-v1-{}",
                                "d".repeat(64)
                            ))
                            .unwrap()
                        ),
                        Some(invalid.to_owned())
                    )
                    .is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn manual_merge_label_is_stable_and_rejects_already_mapped_members() {
        let directory = tempdir().unwrap();
        let store = store(&directory);
        let initial = store.load_or_create().unwrap();
        let first = store
            .resolve_or_create(initial.revision(), observation(NODE_ONE, 'a', "zeta"))
            .unwrap();
        let second = store
            .resolve_or_create(
                first.mappings().revision(),
                observation(NODE_TWO, 'b', "alpha"),
            )
            .unwrap();
        let ids = vec![first.instance_id().clone(), second.instance_id().clone()];
        let label = manual_merge_display_label(second.mappings(), &ids).unwrap();
        assert_eq!(label.as_str(), "alpha");
        let merged = store
            .merge_instances(second.mappings().revision(), None, Some(label), &ids)
            .unwrap();
        assert!(manual_merge_display_label(merged.mappings(), &ids).is_err());
    }
}
