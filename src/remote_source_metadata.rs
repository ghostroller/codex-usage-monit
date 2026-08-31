//! Two-phase publication of SSH source metadata.
//!
//! Preparing a sync may create an otherwise empty SSH source or refresh its
//! display label, but it never changes which redaction namespace contributes
//! to aggregates. Publication happens only after a successful sync and only
//! when the target namespace has an active generation bound to the exact
//! configured remote source and current protocol revisions.

use std::io;

use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus, TryWriterLease};
use crate::history_runtime::HistoryRuntime;
use crate::remote_agent::current_revisions;
use crate::remote_ingest_state::{
    purge_remote_ingest_state_for_source, queue_remote_preview_ingest_retirement,
    retry_remote_preview_ingest_retirement,
};
use crate::remote_protocol::SourceGeneration;
use crate::remote_sync::RemoteSyncHostSnapshot;
use crate::remotes_config::{RemotesConfig, RemotesConfigMutation, RemotesConfigStore};
use crate::source_history::{RedactionProfile, SourceHistoryWriter, SourceKind, SourceMetadata};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSourceMetadataPrepareOutcome {
    Created(SourceMetadata),
    Existing(SourceMetadata),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSourceMetadataFinalizeOutcome {
    Published(SourceMetadata),
    /// A bootstrap/identity transition has not published an exact active
    /// generation yet. The caller may continue normally and retry publication
    /// after the next successful bounded sync.
    DeferredUntilMatchingActiveGeneration,
}

/// Durable source transition performed before an SSH identity pin is released
/// or its allowlist entry is removed. Missing metadata is a normal state for a
/// host that was paired but never synchronized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSourceMetadataDetachOutcome {
    HostWasUnpaired,
    MetadataNotFound(SourceGeneration),
    Detached(SourceMetadata),
}

/// Result of one fail-closed connection removal. Source history is never
/// deleted; `config` is the committed allowlist after removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHostRemovalOutcome {
    config: RemotesConfig,
    source_metadata: RemoteSourceMetadataDetachOutcome,
}

/// Result of one fail-closed identity-pin release. The allowlist entry remains
/// configured but disabled, and its released source can no longer be mistaken
/// for an attached synchronization target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHostUnpairOutcome {
    config: RemotesConfig,
    source_metadata: RemoteSourceMetadataDetachOutcome,
}

impl RemoteHostUnpairOutcome {
    pub fn config(&self) -> &RemotesConfig {
        &self.config
    }

    pub fn source_metadata(&self) -> &RemoteSourceMetadataDetachOutcome {
        &self.source_metadata
    }
}

impl RemoteHostRemovalOutcome {
    pub fn config(&self) -> &RemotesConfig {
        &self.config
    }

    pub fn into_config(self) -> RemotesConfig {
        self.config
    }

    pub fn source_metadata(&self) -> &RemoteSourceMetadataDetachOutcome {
        &self.source_metadata
    }
}

/// Result of explicitly reattaching an already-persisted SSH source after a
/// successful pair. Aggregate inclusion is intentionally left unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSourceMetadataReattachOutcome {
    MetadataNotFound(SourceGeneration),
    Reattached(SourceMetadata),
}

/// Result of one explicit, irreversible detached-SSH source purge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteSourcePurgeOutcome {
    ingest_namespaces_removed: usize,
    project_instances_removed: usize,
    resumed_history_purge: bool,
}

impl RemoteSourcePurgeOutcome {
    pub fn ingest_namespaces_removed(self) -> usize {
        self.ingest_namespaces_removed
    }

    pub fn resumed_history_purge(self) -> bool {
        self.resumed_history_purge
    }

    pub fn project_instances_removed(self) -> usize {
        self.project_instances_removed
    }
}

/// Irreversibly removes one detached SSH source's retained history and its
/// source-scoped cursor/WAL state.
///
/// The remotes config lock is held across the complete operation, so a source
/// cannot be paired concurrently after the detached check. Eligibility is
/// validated before ingest state is touched; history is isolated last so a
/// crash can always resume from its durable source-purge marker.
pub fn purge_detached_remote_source(
    config_store: &RemotesConfigStore,
    runtime: &HistoryRuntime,
    source_id: &crate::source_identity::NodeId,
) -> io::Result<RemoteSourcePurgeOutcome> {
    if runtime.source_identity().node_id() == source_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the local machine history source cannot be purged as a remote source",
        ));
    }
    config_store.with_unattached_source(source_id, || {
        with_v2_writer(runtime, |writer| {
            writer.prepare_detached_ssh_source_for_purge(source_id)?;
            let ingest =
                purge_remote_ingest_state_for_source(runtime.source_history(), source_id, writer)?;
            let project_instances_removed = runtime
                .project_mapping_store()
                .purge_source_observations(source_id)?;
            let history = writer.purge_detached_ssh_source(source_id)?;
            Ok(RemoteSourcePurgeOutcome {
                ingest_namespaces_removed: ingest.namespaces_removed,
                project_instances_removed,
                resumed_history_purge: history.resumed_from_trash(),
            })
        })
    })
}

/// Changes only whether one persisted SSH source contributes to aggregate
/// queries. Connection attachment and retained history are independent.
pub fn set_remote_source_in_aggregates(
    runtime: &HistoryRuntime,
    source_id: &crate::source_identity::NodeId,
    include: bool,
) -> io::Result<SourceMetadata> {
    let metadata = runtime.source_history().load_source_metadata(source_id)?;
    if metadata.kind() != SourceKind::Ssh {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote source policy can only be changed for SSH sources",
        ));
    }
    with_v2_writer(runtime, |writer| {
        writer.update_source_metadata(source_id, |metadata| {
            if metadata.kind() != SourceKind::Ssh {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote source identity changed kind during policy update",
                ));
            }
            metadata.set_include_in_aggregates(include);
            Ok(())
        })
    })
}

/// Removes one connection and transitions any already-persisted SSH source to
/// a detached state before publishing the allowlist deletion.
///
/// The complete transition is linearized under the remotes config exclusive
/// lock. The lock order is always `config -> v2 history writer`. If metadata
/// persistence succeeds but config publication fails, the host remains
/// configured while its source is safely detached and, by default, excluded.
/// No history files are deleted. `keep_included` preserves the source's
/// existing aggregate-inclusion choice instead of forcing it to false.
pub fn remove_remote_host_with_source_policy(
    config_store: &RemotesConfigStore,
    expected_revision: u64,
    host_id: &str,
    runtime: &HistoryRuntime,
    keep_included: bool,
) -> io::Result<RemoteHostRemovalOutcome> {
    let (config, source_metadata) = config_store.update_after_precommit(
        expected_revision,
        RemotesConfigMutation::remove_host(host_id.to_owned()),
        |previous, _candidate| {
            let host = previous.host(host_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote host {host_id:?} is not configured"),
                )
            })?;
            let Some(source) = host.expected_source().or_else(|| host.previous_source()) else {
                return Ok(RemoteSourceMetadataDetachOutcome::HostWasUnpaired);
            };
            detach_persisted_ssh_source(runtime, source, keep_included)
        },
    )?;
    Ok(RemoteHostRemovalOutcome {
        config,
        source_metadata,
    })
}

/// Releases one active identity pin only after its persisted SSH source is
/// durably detached. The released source reference remains in the disabled
/// host row so a later `remove` can still apply its default exclusion policy.
///
/// Unpairing is primarily an identity-rotation recovery step, so it preserves
/// `include_in_aggregates`. The complete transition is linearized under the
/// remotes config lock in the same `config -> v2 history writer` order used by
/// removal. A metadata failure leaves the config byte-for-byte unchanged. If
/// config publication itself fails after detachment, the old active pin stays
/// configured while the source remains fail-closed and a retry is safe.
pub fn unpair_remote_host_with_source_policy(
    config_store: &RemotesConfigStore,
    expected_revision: u64,
    host_id: &str,
    runtime: &HistoryRuntime,
) -> io::Result<RemoteHostUnpairOutcome> {
    let (config, source_metadata) = config_store.update_after_precommit(
        expected_revision,
        RemotesConfigMutation::unpair_host(host_id.to_owned()),
        |previous, _candidate| {
            let host = previous.host(host_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote host {host_id:?} is not configured"),
                )
            })?;
            let Some(source) = host.expected_source().or_else(|| host.previous_source()) else {
                return Ok(RemoteSourceMetadataDetachOutcome::HostWasUnpaired);
            };
            detach_persisted_ssh_source(runtime, source, true)
        },
    )?;
    Ok(RemoteHostUnpairOutcome {
        config,
        source_metadata,
    })
}

/// Clears `detached` for the exact currently-paired source while preserving
/// `include_in_aggregates` byte-for-byte.
///
/// This is deliberately a separate explicit-pair completion step; ordinary
/// synchronization must not silently revive a source detached by removal.
/// A concurrent remove either wins before the exact-host check, or waits and
/// applies the final detach after this short update.
pub fn reattach_remote_source_metadata_if_current(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
) -> io::Result<RemoteSourceMetadataReattachOutcome> {
    config_store.with_current_host(
        selected.config_revision(),
        selected.host(),
        || -> io::Result<RemoteSourceMetadataReattachOutcome> {
            let source = selected_source_not_local(selected, runtime)?;
            match runtime
                .source_history()
                .load_source_metadata(&source.node_id)
            {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(
                    RemoteSourceMetadataReattachOutcome::MetadataNotFound(source.clone()),
                ),
                Err(error) => Err(error),
                Ok(metadata) if metadata.kind() != SourceKind::Ssh => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "paired remote source identity collides with a local history source",
                )),
                Ok(_) => with_v2_writer(runtime, |writer| {
                    let metadata = writer.update_source_metadata(&source.node_id, |metadata| {
                        if metadata.kind() != SourceKind::Ssh {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "paired remote source identity collides with a local history source",
                            ));
                        }
                        metadata.set_display_label(selected.host().id())?;
                        metadata.set_detached(false);
                        Ok(())
                    })?;
                    Ok(RemoteSourceMetadataReattachOutcome::Reattached(metadata))
                }),
            }
        },
    )
}

/// Ensures the selected source can receive data without changing the current
/// aggregate profile of an existing SSH source.
pub fn prepare_remote_source_metadata(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
) -> io::Result<RemoteSourceMetadataPrepareOutcome> {
    let source = selected_source_not_local(selected, runtime)?;
    let target_profile = selected_redaction_profile(selected);
    with_exact_v2_writer(config_store, selected, runtime, |writer| {
        match runtime
            .source_history()
            .load_source_metadata(&source.node_id)
        {
            Ok(metadata) if metadata.kind() == SourceKind::Ssh => {
                // Preserve aggregate_redaction_profile, include, and detached.
                // Only the non-semantic display label may change before sync.
                let metadata = writer.update_source_metadata(&source.node_id, |metadata| {
                    metadata.set_display_label(selected.host().id())
                })?;
                if target_profile == RedactionProfile::Redacted {
                    // A prior successful publication may have crashed or hit
                    // Windows sharing semantics during physical preview
                    // retirement. Make one bounded pass before networking;
                    // this never changes aggregate visibility.
                    let _ = writer.retry_remote_source_redaction_retirement(&source.node_id)?;
                    let _ = retry_remote_preview_ingest_retirement(
                        runtime.source_history(),
                        &source.node_id,
                        writer,
                    )?;
                }
                Ok(RemoteSourceMetadataPrepareOutcome::Existing(metadata))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "paired remote source identity collides with a local history source",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let metadata = SourceMetadata::new_with_redaction_profile(
                    source.node_id.clone(),
                    SourceKind::Ssh,
                    selected.host().id(),
                    target_profile,
                )?;
                writer.save_source_metadata(&metadata)?;
                if target_profile == RedactionProfile::Redacted {
                    let _ = retry_remote_preview_ingest_retirement(
                        runtime.source_history(),
                        &source.node_id,
                        writer,
                    )?;
                }
                Ok(RemoteSourceMetadataPrepareOutcome::Created(metadata))
            }
            Err(error) => Err(error),
        }
    })
}

/// Publishes the selected aggregate profile after a successful sync.
///
/// Existing user policy (`include_in_aggregates` and `detached`) is preserved.
/// A missing or stale active generation is a normal deferred outcome for a
/// bounded multi-page bootstrap, not permission to expose older data.
pub fn finalize_remote_source_metadata(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
) -> io::Result<RemoteSourceMetadataFinalizeOutcome> {
    let source = selected_source_not_local(selected, runtime)?;
    let target_profile = selected_redaction_profile(selected);
    with_exact_v2_writer(config_store, selected, runtime, |writer| {
        let metadata = runtime
            .source_history()
            .load_source_metadata(&source.node_id)?;
        if metadata.kind() != SourceKind::Ssh {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "paired remote source identity collides with a local history source",
            ));
        }

        let Some(active) = runtime
            .source_history()
            .active_remote_history_ref(&source.node_id, target_profile)?
        else {
            return Ok(RemoteSourceMetadataFinalizeOutcome::DeferredUntilMatchingActiveGeneration);
        };
        if active.binding().source() != source
            || active.binding().revisions() != &current_revisions()
        {
            return Ok(RemoteSourceMetadataFinalizeOutcome::DeferredUntilMatchingActiveGeneration);
        }

        if target_profile == RedactionProfile::Redacted {
            let _ = queue_remote_preview_ingest_retirement(
                runtime.source_history(),
                &source.node_id,
                writer,
            )?;
        }
        writer.update_source_metadata(&source.node_id, |metadata| {
            if metadata.kind() != SourceKind::Ssh {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "paired remote source identity collides with a local history source",
                ));
            }
            metadata.set_display_label(selected.host().id())?;
            Ok(())
        })?;
        let (metadata, _retirement) =
            writer.publish_remote_source_redaction_profile(&source.node_id, target_profile)?;
        if target_profile == RedactionProfile::Redacted {
            let _ = retry_remote_preview_ingest_retirement(
                runtime.source_history(),
                &source.node_id,
                writer,
            )?;
        }
        Ok(RemoteSourceMetadataFinalizeOutcome::Published(metadata))
    })
}

fn with_exact_v2_writer<T>(
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
    operation: impl FnOnce(&SourceHistoryWriter<'_, '_, '_>) -> io::Result<T>,
) -> io::Result<T> {
    config_store.with_current_host(selected.config_revision(), selected.host(), || {
        with_v2_writer(runtime, operation)
    })
}

fn with_v2_writer<T>(
    runtime: &HistoryRuntime,
    operation: impl FnOnce(&SourceHistoryWriter<'_, '_, '_>) -> io::Result<T>,
) -> io::Result<T> {
    let lease = match runtime.ownership().try_acquire_writer_lease()? {
        TryWriterLease::Acquired(lease) => lease,
        TryWriterLease::Busy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer is busy; retry remote source metadata publication later",
            ));
        }
    };
    let manifest = match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active =>
        {
            manifest
        }
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote source metadata requires active v2 history ownership",
            ));
        }
    };
    let authority = runtime.ownership().authorize_v2_write(&lease, &manifest)?;
    let writer = runtime.source_history().writer(&authority)?;
    let result = operation(&writer)?;
    writer.validate()?;
    Ok(result)
}

fn detach_persisted_ssh_source(
    runtime: &HistoryRuntime,
    source: &SourceGeneration,
    keep_included: bool,
) -> io::Result<RemoteSourceMetadataDetachOutcome> {
    if runtime.source_identity().node_id() == &source.node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paired remote source identity collides with the local machine identity",
        ));
    }
    match runtime
        .source_history()
        .load_source_metadata(&source.node_id)
    {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(
            RemoteSourceMetadataDetachOutcome::MetadataNotFound(source.clone()),
        ),
        Err(error) => Err(error),
        Ok(metadata) if metadata.kind() != SourceKind::Ssh => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paired remote source identity collides with a local history source",
        )),
        Ok(_) => with_v2_writer(runtime, |writer| {
            let metadata = writer.update_source_metadata(&source.node_id, |metadata| {
                if metadata.kind() != SourceKind::Ssh {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "paired remote source identity collides with a local history source",
                    ));
                }
                metadata.set_detached(true);
                if !keep_included {
                    metadata.set_include_in_aggregates(false);
                }
                Ok(())
            })?;
            Ok(RemoteSourceMetadataDetachOutcome::Detached(metadata))
        }),
    }
}

fn selected_source_not_local<'a>(
    selected: &'a RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
) -> io::Result<&'a crate::remote_protocol::SourceGeneration> {
    let source = selected.host().expected_source().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote sync selected an unpaired host",
        )
    })?;
    if runtime.source_identity().node_id() == &source.node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paired remote source identity collides with the local machine identity",
        ));
    }
    Ok(source)
}

fn selected_redaction_profile(selected: &RemoteSyncHostSnapshot) -> RedactionProfile {
    if selected.host().redact_content() {
        RedactionProfile::Redacted
    } else {
        RedactionProfile::PreviewEnabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use std::path::PathBuf;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::Utc;
    use tempfile::{TempDir, tempdir};

    use crate::history_ownership::OwnershipManifestStatus;
    use crate::remote_protocol::SourceGeneration;
    use crate::remotes_config::{RemoteHostEdit, RemotesConfig, RemotesConfigMutation};
    use crate::source_history::{SourceHistoryRemoteBinding, SourceHistoryRemoteGenerationId};

    const NODE_A: &str = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NODE_B: &str = "node-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const NODE_C: &str = "node-cccccccccccccccccccccccccccccccc";
    const REMOTE_GENERATION: &str = "ingest-gen-11111111111111111111111111111111";

    struct Fixture {
        _directory: TempDir,
        state_root: PathBuf,
        codex_home: PathBuf,
        store: RemotesConfigStore,
        config: RemotesConfig,
        runtime: HistoryRuntime,
        remote_source: SourceGeneration,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let state_root = directory.path().join("state");
            let codex_home = directory.path().join("codex-home");
            std::fs::create_dir(&codex_home).unwrap();
            let mut runtime =
                HistoryRuntime::new(state_root.join("history-v1"), &codex_home, false).unwrap();
            runtime.ensure_v2_active().unwrap();
            let remote_node = if runtime.source_identity().node_id().as_str() == NODE_A {
                NODE_B
            } else {
                NODE_A
            };
            let remote_source = SourceGeneration {
                node_id: remote_node.parse().unwrap(),
                generation: NonZeroU64::new(1).unwrap(),
            };
            let (store, config) = configured_store(
                directory.path().join("config/remotes.json"),
                remote_source.clone(),
            );
            Self {
                _directory: directory,
                state_root,
                codex_home,
                store,
                config,
                runtime,
                remote_source,
            }
        }

        fn selected(&self) -> RemoteSyncHostSnapshot {
            RemoteSyncHostSnapshot::capture_for_automatic(
                &self.config,
                self.config.host("dev").unwrap(),
            )
            .unwrap()
        }

        fn set_existing_policy(&self, profile: RedactionProfile) {
            let lease = self.runtime.ownership().acquire_writer_lease().unwrap();
            let OwnershipManifestStatus::Initialized(manifest) =
                self.runtime.ownership().load_manifest().unwrap()
            else {
                panic!("fixture ownership is initialized")
            };
            let authority = self
                .runtime
                .ownership()
                .authorize_v2_write(&lease, &manifest)
                .unwrap();
            let writer = self.runtime.source_history().writer(&authority).unwrap();
            writer
                .update_source_metadata(&self.remote_source.node_id, |metadata| {
                    metadata.set_display_label("old-label")?;
                    metadata.set_aggregate_redaction_profile(profile);
                    metadata.set_include_in_aggregates(false);
                    metadata.set_detached(true);
                    Ok(())
                })
                .unwrap();
        }

        fn create_matching_active_generation(&self, profile: RedactionProfile) {
            let lease = self.runtime.ownership().acquire_writer_lease().unwrap();
            let OwnershipManifestStatus::Initialized(manifest) =
                self.runtime.ownership().load_manifest().unwrap()
            else {
                panic!("fixture ownership is initialized")
            };
            let authority = self
                .runtime
                .ownership()
                .authorize_v2_write(&lease, &manifest)
                .unwrap();
            let writer = self.runtime.source_history().writer(&authority).unwrap();
            let generation = REMOTE_GENERATION
                .parse::<SourceHistoryRemoteGenerationId>()
                .unwrap();
            let binding =
                SourceHistoryRemoteBinding::new(self.remote_source.clone(), current_revisions())
                    .unwrap();
            writer
                .ensure_remote_history_generation(
                    &self.remote_source.node_id,
                    profile,
                    &generation,
                    &binding,
                )
                .unwrap();
            writer
                .activate_remote_history_generation(
                    &self.remote_source.node_id,
                    profile,
                    None,
                    &generation,
                    &binding,
                    Utc::now(),
                )
                .unwrap();
        }
    }

    fn configured_store(
        path: PathBuf,
        source: SourceGeneration,
    ) -> (RemotesConfigStore, RemotesConfig) {
        let store = RemotesConfigStore::new(path);
        let mut config = store.load_or_create().unwrap();
        for mutation in [
            RemotesConfigMutation::add_host("dev", "dev-alias"),
            RemotesConfigMutation::pair_pin("dev", source),
            RemotesConfigMutation::edit_host(
                "dev",
                RemoteHostEdit {
                    ssh_host: None,
                    agent_executable: None,
                    redact_content: Some(false),
                },
            ),
            RemotesConfigMutation::enable_host("dev"),
            RemotesConfigMutation::set_auto_sync_enabled(true),
        ] {
            config = store.update(config.config_revision(), mutation).unwrap();
        }
        (store, config)
    }

    #[test]
    fn prepare_existing_ssh_source_preserves_aggregate_and_user_policy() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        assert!(matches!(
            prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap(),
            RemoteSourceMetadataPrepareOutcome::Created(_)
        ));
        fixture.set_existing_policy(RedactionProfile::Redacted);

        let outcome =
            prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();

        let RemoteSourceMetadataPrepareOutcome::Existing(metadata) = outcome else {
            panic!("existing metadata should be updated in place")
        };
        assert_eq!(metadata.display_label(), "dev");
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );
        assert!(!metadata.include_in_aggregates());
        assert!(metadata.detached());
    }

    #[test]
    fn purge_refuses_a_source_still_pinned_by_config() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();

        let error = purge_detached_remote_source(
            &fixture.store,
            &fixture.runtime,
            &fixture.remote_source.node_id,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            fixture
                .runtime
                .source_history()
                .source_directory(&fixture.remote_source.node_id)
                .is_dir()
        );
    }

    #[test]
    fn purge_of_removed_host_stays_absent_after_runtime_restart() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let removed = remove_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
            false,
        )
        .unwrap();
        assert!(removed.config().host("dev").is_none());

        purge_detached_remote_source(
            &fixture.store,
            &fixture.runtime,
            &fixture.remote_source.node_id,
        )
        .unwrap();
        assert!(
            !fixture
                .runtime
                .source_history()
                .source_directory(&fixture.remote_source.node_id)
                .exists()
        );

        let restarted = HistoryRuntime::new(
            fixture.state_root.join("history-v1"),
            &fixture.codex_home,
            false,
        )
        .unwrap();
        let error = restarted
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn finalize_requires_matching_active_generation_then_flips_atomically() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        fixture.set_existing_policy(RedactionProfile::Redacted);

        assert_eq!(
            finalize_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap(),
            RemoteSourceMetadataFinalizeOutcome::DeferredUntilMatchingActiveGeneration
        );
        fixture.create_matching_active_generation(RedactionProfile::PreviewEnabled);

        let outcome =
            finalize_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let RemoteSourceMetadataFinalizeOutcome::Published(metadata) = outcome else {
            panic!("matching active generation should publish metadata")
        };
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::PreviewEnabled
        );
        assert_eq!(metadata.display_label(), "dev");
        assert!(!metadata.include_in_aggregates());
        assert!(metadata.detached());
    }

    #[test]
    fn finalized_preview_to_redacted_switch_retires_old_preview_namespace() {
        let mut fixture = Fixture::new();
        let preview_selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &preview_selected, &fixture.runtime)
            .unwrap();
        fixture.create_matching_active_generation(RedactionProfile::PreviewEnabled);
        finalize_remote_source_metadata(&fixture.store, &preview_selected, &fixture.runtime)
            .unwrap();
        let preview_directory = fixture
            .runtime
            .source_history()
            .source_directory(&fixture.remote_source.node_id)
            .join(RedactionProfile::PreviewEnabled.directory_name());
        assert!(preview_directory.is_dir());

        fixture.config = fixture
            .store
            .update(
                fixture.config.config_revision(),
                RemotesConfigMutation::edit_host(
                    "dev",
                    RemoteHostEdit {
                        ssh_host: None,
                        agent_executable: None,
                        redact_content: Some(true),
                    },
                ),
            )
            .unwrap();
        fixture.runtime = HistoryRuntime::new(
            fixture.state_root.join("history-v1"),
            &fixture.codex_home,
            true,
        )
        .unwrap();
        fixture.runtime.ensure_v2_active().unwrap();
        let redacted_selected = fixture.selected();

        // Preparation cannot remove the namespace that source metadata still
        // exposes; only a matching redacted active generation authorizes the
        // publication and retirement below.
        prepare_remote_source_metadata(&fixture.store, &redacted_selected, &fixture.runtime)
            .unwrap();
        assert!(preview_directory.is_dir());
        fixture.create_matching_active_generation(RedactionProfile::Redacted);

        let outcome =
            finalize_remote_source_metadata(&fixture.store, &redacted_selected, &fixture.runtime)
                .unwrap();
        let RemoteSourceMetadataFinalizeOutcome::Published(metadata) = outcome else {
            panic!("matching redacted generation should publish metadata")
        };
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );
        assert!(!preview_directory.exists());
    }

    #[test]
    fn stale_config_cannot_publish_target_profile() {
        let mut fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        fixture.set_existing_policy(RedactionProfile::Redacted);
        fixture.create_matching_active_generation(RedactionProfile::PreviewEnabled);
        fixture.config = fixture
            .store
            .update(
                fixture.config.config_revision(),
                RemotesConfigMutation::disable_host("dev"),
            )
            .unwrap();

        let error = finalize_remote_source_metadata(&fixture.store, &selected, &fixture.runtime)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let metadata = fixture
            .runtime
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert_eq!(
            metadata.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );
        assert!(!metadata.include_in_aggregates());
        assert!(metadata.detached());
    }

    #[test]
    fn remove_detaches_excludes_and_preserves_history_across_restart() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let source_directory = fixture
            .runtime
            .source_history()
            .source_directory(&fixture.remote_source.node_id);
        assert!(source_directory.is_dir());

        let removed = remove_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
            false,
        )
        .unwrap();

        assert!(removed.config().host("dev").is_none());
        let RemoteSourceMetadataDetachOutcome::Detached(metadata) = removed.source_metadata()
        else {
            panic!("an existing SSH source must be detached before config removal")
        };
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
        assert!(
            source_directory.is_dir(),
            "source history must not be deleted"
        );

        drop(fixture.runtime);
        let restarted = HistoryRuntime::new(
            fixture.state_root.join("history-v1"),
            &fixture.codex_home,
            false,
        )
        .unwrap();
        let metadata = restarted
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
        assert!(
            restarted
                .source_history()
                .source_directory(&fixture.remote_source.node_id)
                .is_dir()
        );
    }

    #[test]
    fn unpair_detaches_but_preserves_include_then_remove_can_still_exclude() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();

        let unpaired = unpair_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
        )
        .unwrap();

        let host = unpaired.config().host("dev").unwrap();
        assert_eq!(host.expected_source(), None);
        assert_eq!(host.previous_source(), Some(&fixture.remote_source));
        assert!(!host.sync_enabled());
        let RemoteSourceMetadataDetachOutcome::Detached(metadata) = unpaired.source_metadata()
        else {
            panic!("a synchronized source must be detached before its pin is released")
        };
        assert!(metadata.detached());
        assert!(
            metadata.include_in_aggregates(),
            "identity recovery must not hide retained history"
        );

        let removed = remove_remote_host_with_source_policy(
            &fixture.store,
            unpaired.config().config_revision(),
            "dev",
            &fixture.runtime,
            false,
        )
        .unwrap();
        assert!(removed.config().host("dev").is_none());
        let RemoteSourceMetadataDetachOutcome::Detached(metadata) = removed.source_metadata()
        else {
            panic!("remove must retain the released source reference long enough to exclude it")
        };
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());

        drop(fixture.runtime);
        let restarted = HistoryRuntime::new(
            fixture.state_root.join("history-v1"),
            &fixture.codex_home,
            false,
        )
        .unwrap();
        let metadata = restarted
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
    }

    #[test]
    fn unpair_then_pair_another_source_never_leaves_the_old_source_attached() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let unpaired = unpair_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
        )
        .unwrap();
        let replacement = SourceGeneration {
            node_id: NODE_C.parse().unwrap(),
            generation: NonZeroU64::new(2).unwrap(),
        };
        let repaired = fixture
            .store
            .update(
                unpaired.config().config_revision(),
                RemotesConfigMutation::pair_pin("dev", replacement.clone()),
            )
            .unwrap();
        let host = repaired.host("dev").unwrap();
        assert_eq!(host.expected_source(), Some(&replacement));
        assert_eq!(host.previous_source(), None);
        let replacement_selected = RemoteSyncHostSnapshot::capture_manual(&repaired, host).unwrap();
        assert!(matches!(
            reattach_remote_source_metadata_if_current(
                &fixture.store,
                &replacement_selected,
                &fixture.runtime,
            )
            .unwrap(),
            RemoteSourceMetadataReattachOutcome::MetadataNotFound(source) if source == replacement
        ));
        prepare_remote_source_metadata(&fixture.store, &replacement_selected, &fixture.runtime)
            .unwrap();

        let old = fixture
            .runtime
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(old.detached());
        assert!(old.include_in_aggregates());
        let new = fixture
            .runtime
            .source_history()
            .load_source_metadata(&replacement.node_id)
            .unwrap();
        assert!(!new.detached());
    }

    #[test]
    fn remove_keep_included_and_missing_metadata_are_explicit_normal_outcomes() {
        let included = Fixture::new();
        let selected = included.selected();
        prepare_remote_source_metadata(&included.store, &selected, &included.runtime).unwrap();
        let removed = remove_remote_host_with_source_policy(
            &included.store,
            included.config.config_revision(),
            "dev",
            &included.runtime,
            true,
        )
        .unwrap();
        let RemoteSourceMetadataDetachOutcome::Detached(metadata) = removed.source_metadata()
        else {
            panic!("prepared metadata should exist")
        };
        assert!(metadata.detached());
        assert!(metadata.include_in_aggregates());

        let missing = Fixture::new();
        let removed = remove_remote_host_with_source_policy(
            &missing.store,
            missing.config.config_revision(),
            "dev",
            &missing.runtime,
            false,
        )
        .unwrap();
        assert!(removed.config().host("dev").is_none());
        assert_eq!(
            removed.source_metadata(),
            &RemoteSourceMetadataDetachOutcome::MetadataNotFound(missing.remote_source.clone())
        );
    }

    #[test]
    fn config_publish_failure_leaves_configured_source_fail_closed_after_restart() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        fixture.store.fail_next_precommitted_write_for_test();

        let error = remove_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(fixture.store.load().unwrap().host("dev").is_some());

        drop(fixture.runtime);
        let restarted = HistoryRuntime::new(
            fixture.state_root.join("history-v1"),
            &fixture.codex_home,
            false,
        )
        .unwrap();
        let metadata = restarted
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(!metadata.include_in_aggregates());
    }

    #[test]
    fn unpair_metadata_failure_does_not_publish_the_pin_release() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let _busy_writer = fixture.runtime.ownership().acquire_writer_lease().unwrap();

        let error = unpair_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let current = fixture.store.load().unwrap();
        let host = current.host("dev").unwrap();
        assert_eq!(host.expected_source(), Some(&fixture.remote_source));
        assert_eq!(host.previous_source(), None);
        assert!(host.sync_enabled());
        let metadata = fixture
            .runtime
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(!metadata.detached());
        assert!(metadata.include_in_aggregates());
    }

    #[test]
    fn unpair_config_publish_failure_keeps_the_active_pin_and_detaches_safely() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        fixture.store.fail_next_precommitted_write_for_test();

        let error = unpair_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        let current = fixture.store.load().unwrap();
        let host = current.host("dev").unwrap();
        assert_eq!(host.expected_source(), Some(&fixture.remote_source));
        assert_eq!(host.previous_source(), None);
        let metadata = fixture
            .runtime
            .source_history()
            .load_source_metadata(&fixture.remote_source.node_id)
            .unwrap();
        assert!(metadata.detached());
        assert!(metadata.include_in_aggregates());
    }

    #[test]
    fn explicit_repair_same_node_clears_detached_and_preserves_include() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        let removed = remove_remote_host_with_source_policy(
            &fixture.store,
            fixture.config.config_revision(),
            "dev",
            &fixture.runtime,
            false,
        )
        .unwrap();

        let mut repaired = fixture
            .store
            .update(
                removed.config().config_revision(),
                RemotesConfigMutation::add_host("dev", "dev-alias"),
            )
            .unwrap();
        repaired = fixture
            .store
            .update(
                repaired.config_revision(),
                RemotesConfigMutation::pair_pin("dev", fixture.remote_source.clone()),
            )
            .unwrap();
        let selected =
            RemoteSyncHostSnapshot::capture_manual(&repaired, repaired.host("dev").unwrap())
                .unwrap();
        let outcome =
            reattach_remote_source_metadata_if_current(&fixture.store, &selected, &fixture.runtime)
                .unwrap();
        let RemoteSourceMetadataReattachOutcome::Reattached(metadata) = outcome else {
            panic!("the old source metadata should be reused")
        };
        assert!(!metadata.detached());
        assert!(
            !metadata.include_in_aggregates(),
            "explicit re-pair must preserve the prior include policy"
        );
        assert_eq!(
            fixture
                .runtime
                .source_history()
                .list_source_metadata()
                .unwrap()
                .iter()
                .filter(|metadata| metadata.source_id() == &fixture.remote_source.node_id)
                .count(),
            1,
            "re-pairing the same NodeId must not create a duplicate source"
        );
        assert!(!repaired.host("dev").unwrap().sync_enabled());
    }

    #[test]
    fn explicit_include_policy_is_independent_from_attachment_and_history() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();
        fixture.set_existing_policy(RedactionProfile::Redacted);

        let included =
            set_remote_source_in_aggregates(&fixture.runtime, &fixture.remote_source.node_id, true)
                .unwrap();
        assert!(included.include_in_aggregates());
        assert!(included.detached());
        assert_eq!(
            included.aggregate_redaction_profile(),
            RedactionProfile::Redacted
        );

        let excluded = set_remote_source_in_aggregates(
            &fixture.runtime,
            &fixture.remote_source.node_id,
            false,
        )
        .unwrap();
        assert!(!excluded.include_in_aggregates());
        assert!(excluded.detached());
        assert_eq!(
            fixture
                .runtime
                .source_history()
                .load_source_metadata(&fixture.remote_source.node_id)
                .unwrap(),
            excluded
        );
    }

    #[test]
    fn exclusive_config_lock_spans_detach_through_allowlist_publish() {
        let fixture = Fixture::new();
        let selected = fixture.selected();
        prepare_remote_source_metadata(&fixture.store, &selected, &fixture.runtime).unwrap();

        let store = Arc::new(fixture.store.clone());
        let removing_store = Arc::clone(&store);
        let revision = fixture.config.config_revision();
        let runtime = fixture.runtime;
        let (detached_tx, detached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let remover = thread::spawn(move || {
            removing_store.update_after_precommit(
                revision,
                RemotesConfigMutation::remove_host("dev"),
                |previous, _candidate| {
                    let source = previous.host("dev").unwrap().expected_source().unwrap();
                    let outcome = detach_persisted_ssh_source(&runtime, source, false)?;
                    detached_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(outcome)
                },
            )
        });
        detached_rx.recv().unwrap();

        let loading_store = Arc::clone(&store);
        let (loading_tx, loading_rx) = mpsc::channel();
        let (loaded_tx, loaded_rx) = mpsc::channel();
        let loader = thread::spawn(move || {
            loading_tx.send(()).unwrap();
            loaded_tx.send(loading_store.load()).unwrap();
        });
        loading_rx.recv().unwrap();
        assert!(
            loaded_rx
                .recv_timeout(StdDuration::from_millis(100))
                .is_err(),
            "a reader must not observe the precommit detach before config removal publishes"
        );
        release_tx.send(()).unwrap();
        let (removed, outcome) = remover.join().unwrap().unwrap();
        assert!(removed.host("dev").is_none());
        assert!(matches!(
            outcome,
            RemoteSourceMetadataDetachOutcome::Detached(_)
        ));
        assert!(loaded_rx.recv().unwrap().unwrap().host("dev").is_none());
        loader.join().unwrap();
    }
}
