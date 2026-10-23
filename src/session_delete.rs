//! Explicit local session deletion. Retained usage is committed before the
//! official RPC; this module never removes or rewrites Codex session files.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};

use crate::app_server::session_management::{
    ManagedThread, SessionManagementClient, is_missing_thread,
};
use crate::config::CollectConfig;
use crate::deleted_sessions::{self, DeletedSessionRecord, DeletionStatus};
use crate::domain::{TaskStatus, TokenUsage, TurnStatus};
use crate::rollout::{SessionUsageCapture, capture_session_usage};

#[cfg(all(test, unix))]
#[path = "session_delete/protocol_tests.rs"]
mod protocol_tests;

#[derive(Clone, Debug)]
pub(crate) struct PreparedSessionDeletion {
    pub(crate) root_thread_id: String,
    pub(crate) thread_ids: Vec<String>,
    pub(crate) captured_at: DateTime<Utc>,
    pub(crate) total_tokens: TokenUsage,
    selected_root: String,
    capture_boundary: DateTime<Utc>,
    binding: LocalBinding,
    threads: Vec<ManagedThread>,
    capture: SessionUsageCapture,
    operation_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeletionOutcome {
    pub(crate) deleted_threads: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalBinding {
    home: PathBuf,
    program: PathBuf,
    program_len: u64,
    program_modified: SystemTime,
    path: Option<OsString>,
    #[cfg(unix)]
    program_identity: (u64, u64),
}

impl LocalBinding {
    fn resolve(config: &CollectConfig) -> Result<Self> {
        ensure!(
            !config.offline,
            "session deletion is unavailable in offline mode"
        );
        let home =
            fs::canonicalize(&config.codex_home).context("local Codex home is unavailable")?;
        ensure!(home.is_dir(), "local Codex home is not a directory");
        let path = config
            .app_server_path
            .clone()
            .or_else(|| std::env::var_os("PATH"));
        let program = crate::app_server::session_management::resolve_executable(config)?;
        let metadata = fs::metadata(&program)?;
        ensure!(
            metadata.is_file(),
            "Codex CLI must be a local executable file"
        );
        Ok(Self {
            home,
            program,
            program_len: metadata.len(),
            program_modified: metadata.modified()?,
            path,
            #[cfg(unix)]
            program_identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        })
    }

    fn bound_config(&self, config: &CollectConfig) -> CollectConfig {
        let mut bound = config.clone();
        bound.codex_home = self.home.clone();
        bound.codex_bin = Some(self.program.clone());
        bound.app_server_path = self.path.clone();
        bound
    }
}

/// Reads fresh local state and captures complete, content-free own usage for
/// exactly the root and the persisted spawned descendants being confirmed.
pub(crate) fn prepare_session_delete(
    config: &CollectConfig,
    raw_uuid: &str,
) -> Result<PreparedSessionDeletion> {
    validate_uuid(raw_uuid)?;
    let binding = LocalBinding::resolve(config)?;
    let bound = binding.bound_config(config);
    let mut client = SessionManagementClient::connect(&bound)?;
    prepare_with_client(&bound, raw_uuid, binding, &mut client)
}

/// Persists all captured members before issuing one `thread/delete`. A failed
/// or missing response retains the journal and never retries the mutation.
pub(crate) fn execute_session_delete(
    config: &CollectConfig,
    prepared: PreparedSessionDeletion,
) -> Result<DeletionOutcome> {
    validate_prepared_summary(&prepared)?;
    ensure!(
        LocalBinding::resolve(config)? == prepared.binding,
        "Codex home, CLI executable or PATH changed; prepare deletion again"
    );
    let bound = prepared.binding.bound_config(config);
    let _operation_lock = deleted_sessions::acquire_operation_lock(&bound)?;
    let mut client = SessionManagementClient::connect(&bound)?;
    execute_with_client(&bound, prepared, &mut client)
}

trait ManagementClient {
    fn read_thread(&mut self, id: &str) -> Result<ManagedThread>;
    fn descendants(&mut self, root: &str) -> Result<Vec<ManagedThread>>;
    fn delete_thread(&mut self, root: &str) -> Result<()>;
}

impl ManagementClient for SessionManagementClient {
    fn read_thread(&mut self, id: &str) -> Result<ManagedThread> {
        SessionManagementClient::read_thread(self, id)
    }
    fn descendants(&mut self, root: &str) -> Result<Vec<ManagedThread>> {
        SessionManagementClient::descendants(self, root)
    }
    fn delete_thread(&mut self, root: &str) -> Result<()> {
        SessionManagementClient::delete_thread(self, root)
    }
}

fn prepare_with_client(
    config: &CollectConfig,
    root: &str,
    binding: LocalBinding,
    client: &mut impl ManagementClient,
) -> Result<PreparedSessionDeletion> {
    let threads = fresh_tree(client, root, &binding.home)?;
    let thread_ids = threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<Vec<_>>();
    let targets = thread_ids.iter().cloned().collect::<HashSet<_>>();
    let captured_at = Utc::now();
    let capture = capture_session_usage(config, &targets, captured_at, &AtomicBool::new(false))?;
    validate_capture(&threads, &capture)?;
    ensure!(
        fresh_tree(client, root, &binding.home)? == threads,
        "session state or spawned descendants changed while preparing deletion"
    );
    capture.validate_unchanged()?;
    let mut total_tokens = TokenUsage::default();
    for call in &capture.dataset.calls {
        ensure!(
            targets.contains(&call.thread_id),
            "capture included a different session"
        );
        total_tokens.add_assign(call.tokens);
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        anyhow::anyhow!("could not create deletion operation identity: {error}")
    })?;
    let operation_id = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(PreparedSessionDeletion {
        root_thread_id: root.to_owned(),
        thread_ids,
        captured_at,
        total_tokens,
        selected_root: root.to_owned(),
        capture_boundary: captured_at,
        binding,
        threads,
        capture,
        operation_id,
    })
}

fn execute_with_client(
    config: &CollectConfig,
    prepared: PreparedSessionDeletion,
    client: &mut impl ManagementClient,
) -> Result<DeletionOutcome> {
    validate_prepared_summary(&prepared)?;
    ensure!(
        fresh_tree(client, &prepared.root_thread_id, &prepared.binding.home)? == prepared.threads,
        "session state or spawned descendants changed after confirmation; prepare again"
    );
    prepared.capture.validate_unchanged()?;
    validate_capture(&prepared.threads, &prepared.capture)?;
    let mut records = Vec::with_capacity(prepared.threads.len());
    for thread in &prepared.threads {
        let task = prepared
            .capture
            .dataset
            .tasks
            .iter()
            .find(|task| task.thread_id == thread.id)
            .context("captured session metadata is missing")?;
        let mut record = DeletedSessionRecord::from_dataset(
            &thread.id,
            thread.name.as_deref().unwrap_or(&task.title),
            prepared.captured_at,
            &prepared.capture.dataset,
        )?;
        record.parent_thread_id = thread.parent_thread_id.clone();
        records.push(record);
    }
    // No mutation can occur before this durable all-member transaction.
    deleted_sessions::prepare_delete(config, &prepared.operation_id, &records)?;
    let final_validation = (|| -> Result<()> {
        prepared.capture.validate_unchanged()?;
        ensure!(
            fresh_tree(client, &prepared.root_thread_id, &prepared.binding.home)?
                == prepared.threads,
            "session state or spawned descendants changed before the delete request"
        );
        prepared.capture.validate_unchanged()
    })();
    if let Err(error) = final_validation {
        for id in &prepared.thread_ids {
            deleted_sessions::set_deletion_outcome(
                config,
                &prepared.operation_id,
                id,
                DeletionStatus::Failed,
                Utc::now(),
            )?;
        }
        return Err(error).context("logs changed before deletion; no delete request was sent");
    }
    let deletion_result = client.delete_thread(&prepared.root_thread_id);
    // A restored copy at a different path must not be mistaken for deletion
    // merely because the original path and an index lookup are now missing.
    let remaining_owned = prepared.capture.remaining_owned_threads();
    let mut deleted_threads = Vec::new();
    let mut unresolved = Vec::new();
    for id in &prepared.thread_ids {
        let observation = client.read_thread(id);
        let missing = observation
            .as_ref()
            .err()
            .is_some_and(|error| is_missing_thread(error, id));
        let paths = prepared.capture.paths_for_thread(id);
        let files_absent = remaining_owned.as_ref().is_ok_and(|owners| !owners.contains(id))
            && !paths.is_empty() && paths.iter().all(|path| {
            matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        });
        let status = if missing && files_absent {
            deleted_threads.push(id.clone());
            DeletionStatus::Deleted
        } else {
            unresolved.push(id.clone());
            if observation.is_ok() && deletion_result.is_ok() {
                DeletionStatus::Partial
            } else {
                DeletionStatus::Unknown
            }
        };
        deleted_sessions::set_deletion_outcome(
            config,
            &prepared.operation_id,
            id,
            status,
            Utc::now(),
        )
        .context("deletion was attempted, but its journal outcome could not be committed")?;
    }
    if let Err(error) = deletion_result {
        bail!(
            "deletion request failed or its result is unknown; captured usage remains saved ({} members absent, {} unresolved): {error:#}",
            deleted_threads.len(),
            unresolved.len()
        );
    }
    ensure!(
        unresolved.is_empty(),
        "deletion is only partially confirmed; captured usage remains saved ({} members absent, {} unresolved)",
        deleted_threads.len(),
        unresolved.len()
    );
    Ok(DeletionOutcome { deleted_threads })
}

fn validate_prepared_summary(prepared: &PreparedSessionDeletion) -> Result<()> {
    let ids = prepared
        .threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect::<Vec<_>>();
    let mut tokens = TokenUsage::default();
    for call in &prepared.capture.dataset.calls {
        tokens.add_assign(call.tokens);
    }
    ensure!(
        prepared.root_thread_id == prepared.selected_root
            && prepared.thread_ids == ids
            && prepared.captured_at == prepared.capture_boundary
            && prepared.total_tokens == tokens,
        "prepared deletion summary was modified; prepare and confirm again"
    );
    Ok(())
}

fn fresh_tree(
    client: &mut impl ManagementClient,
    root: &str,
    home: &Path,
) -> Result<Vec<ManagedThread>> {
    validate_uuid(root)?;
    let mut members = BTreeMap::new();
    let root_thread = client.read_thread(root)?;
    ensure!(
        root_thread.id == root,
        "thread/read returned a different root identity"
    );
    members.insert(root.to_owned(), root_thread);
    for listed in client.descendants(root)? {
        validate_uuid(&listed.id)?;
        ensure!(
            listed.id != root && !members.contains_key(&listed.id),
            "invalid duplicate descendant identity"
        );
        let fresh = client.read_thread(&listed.id)?;
        ensure!(
            fresh.parent_thread_id == listed.parent_thread_id && fresh.path == listed.path,
            "descendant ownership changed while reading its metadata"
        );
        members.insert(fresh.id.clone(), fresh);
    }
    for thread in members.values() {
        ensure!(
            thread.id == root || thread.parent_thread_id.is_some(),
            "descendant has no proven spawned parent"
        );
        ensure!(
            !thread.ephemeral,
            "ephemeral sessions cannot be deleted with retained usage"
        );
        ensure!(
            matches!(thread.runtime_status.as_str(), "idle" | "notLoaded"),
            "session {} is active or its runtime state is unavailable",
            thread.id
        );
        let path =
            fs::canonicalize(&thread.path).context("persisted session log is unavailable")?;
        ensure!(
            path.starts_with(home.join("sessions"))
                || path.starts_with(home.join("archived_sessions")),
            "session log is outside the local Codex sessions directories"
        );
        if thread.id != root {
            let mut current = thread;
            let mut seen = BTreeSet::new();
            while current.id != root {
                ensure!(
                    seen.insert(current.id.clone()),
                    "cyclic spawned-session ownership"
                );
                let parent = current
                    .parent_thread_id
                    .as_ref()
                    .context("spawned ownership is incomplete")?;
                current = members
                    .get(parent)
                    .context("descendant is not connected to the selected root")?;
            }
        }
    }
    Ok(members.into_values().collect())
}

fn validate_capture(threads: &[ManagedThread], capture: &SessionUsageCapture) -> Result<()> {
    ensure!(
        capture.paths().len() >= threads.len(),
        "owned session capture is incomplete"
    );
    for thread in threads {
        let task = capture
            .dataset
            .tasks
            .iter()
            .find(|task| task.thread_id == thread.id)
            .context("complete own-session usage metadata could not be captured")?;
        ensure!(
            !matches!(
                task.status,
                TaskStatus::Running
                    | TaskStatus::WaitingApproval
                    | TaskStatus::WaitingInput
                    | TaskStatus::Stale
                    | TaskStatus::Unknown
            ),
            "session {} is active or its persisted state is unknown",
            thread.id
        );
        for turn in capture
            .dataset
            .turns
            .iter()
            .filter(|turn| turn.thread_id == thread.id)
        {
            ensure!(
                matches!(
                    turn.status,
                    TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed
                ),
                "session {} has an unfinished or unverifiable turn",
                thread.id
            );
        }
        let paths = capture.paths_for_thread(&thread.id);
        let canonical = fs::canonicalize(&thread.path)?;
        ensure!(
            paths
                .iter()
                .any(|path| fs::canonicalize(path).ok().as_ref() == Some(&canonical)),
            "API thread identity does not match its owned usage log"
        );
    }
    Ok(())
}

fn validate_uuid(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    ensure!(
        bytes.len() == 36
            && bytes.iter().enumerate().all(|(index, byte)| {
                if [8, 13, 18, 23].contains(&index) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
                }
            }),
        "select a local session by its exact UUID; session names and remote identities are not accepted"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn prepared_session_deletion_for_test(
    thread_id: &str,
    captured_at: DateTime<Utc>,
) -> PreparedSessionDeletion {
    PreparedSessionDeletion {
        root_thread_id: thread_id.to_owned(),
        thread_ids: vec![thread_id.to_owned()],
        captured_at,
        total_tokens: TokenUsage::default(),
        selected_root: thread_id.to_owned(),
        capture_boundary: captured_at,
        binding: LocalBinding {
            home: PathBuf::from("/nonexistent/monit-ui-test-codex-home"),
            program: PathBuf::from("/nonexistent/monit-ui-test-codex"),
            program_len: 0,
            program_modified: SystemTime::UNIX_EPOCH,
            path: None,
            #[cfg(unix)]
            program_identity: (0, 0),
        },
        threads: Vec::new(),
        capture: SessionUsageCapture::empty_for_test(),
        operation_id: "ui-only-never-execute".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::Duration;
    use serde_json::json;

    use super::*;
    use crate::app_server::session_management::ManagementRpcError;

    const ROOT: &str = "11111111-1111-4111-8111-111111111111";
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";

    enum MockDeletion {
        Success,
        Partial,
        Timeout,
        Crash,
    }

    struct MockClient {
        config: CollectConfig,
        threads: BTreeMap<String, ManagedThread>,
        missing: HashSet<String>,
        deletion: MockDeletion,
        calls: usize,
    }

    impl ManagementClient for MockClient {
        fn read_thread(&mut self, id: &str) -> Result<ManagedThread> {
            if self.missing.contains(id) {
                return Err(ManagementRpcError {
                    code: Some(-32600),
                    message: format!("thread not found: {id}"),
                }
                .into());
            }
            self.threads
                .get(id)
                .cloned()
                .context("unexpected mock thread")
        }

        fn descendants(&mut self, root: &str) -> Result<Vec<ManagedThread>> {
            ensure!(root == ROOT, "unexpected mock root");
            Ok(self
                .threads
                .values()
                .filter(|thread| thread.id != root)
                .cloned()
                .collect())
        }

        fn delete_thread(&mut self, root: &str) -> Result<()> {
            ensure!(root == ROOT, "unexpected mock deletion target");
            self.calls += 1;
            let records = deleted_sessions::load_deleted_sessions(&self.config)?;
            ensure!(
                records.len() == self.threads.len(),
                "RPC reached before all-member ledger commit"
            );
            ensure!(
                records
                    .iter()
                    .all(|record| record.status == DeletionStatus::Pending),
                "RPC reached without a durable pending intent"
            );
            ensure!(
                records
                    .iter()
                    .map(|record| record.token_usage().total_tokens)
                    .sum::<u64>()
                    == 20,
                "RPC reached before complete own metrics were saved"
            );
            if matches!(self.deletion, MockDeletion::Crash) {
                panic!("simulated caller crash before a deletion response");
            }
            if matches!(self.deletion, MockDeletion::Timeout) {
                bail!("mock response timed out");
            }
            for thread in self.threads.values() {
                if matches!(self.deletion, MockDeletion::Partial) && thread.id == ROOT {
                    continue;
                }
                // Simulate remote disappearance only inside this test's temp
                // home. Production deletion never unlinks or renames a log.
                let retired = self
                    .config
                    .codex_home
                    .join(format!("retired-{}", thread.id));
                fs::rename(&thread.path, retired)?;
                self.missing.insert(thread.id.clone());
            }
            Ok(())
        }
    }

    fn fixture() -> (tempfile::TempDir, CollectConfig, MockClient) {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("codex-home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join("sessions")).unwrap();
        fs::create_dir(home.join("archived_sessions")).unwrap();
        let config = CollectConfig {
            codex_home: home,
            codex_bin: Some(std::env::current_exe().unwrap()),
            deleted_sessions_dir: Some(directory.path().join("ledger")),
            ..CollectConfig::default()
        };
        let now = Utc::now() - Duration::minutes(10);
        let mut threads = BTreeMap::new();
        for (id, parent, directory) in [
            (ROOT, None, "sessions"),
            (CHILD, Some(ROOT), "archived_sessions"),
        ] {
            let path = config
                .codex_home
                .join(directory)
                .join(format!("rollout-{id}.jsonl"));
            let mut file = fs::File::create(&path).unwrap();
            let turn = format!("turn-{id}");
            let events = [
                json!({"timestamp":now,"type":"session_meta","payload":{"id":id,"source":"cli","cwd":"/test/project"}}),
                json!({"timestamp":now+Duration::seconds(1),"type":"event_msg","payload":{"type":"task_started","turn_id":turn,"thread_id":id}}),
                json!({"timestamp":now+Duration::seconds(2),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}}),
                json!({"timestamp":now+Duration::seconds(3),"type":"event_msg","payload":{"type":"user_message","message":"SECRET PROMPT MUST NOT BE RETAINED","turn_id":turn}}),
                json!({"timestamp":now+Duration::seconds(4),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10},"last_token_usage":{"input_tokens":8,"output_tokens":2,"total_tokens":10}}}}),
                json!({"timestamp":now+Duration::seconds(5),"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}}),
            ];
            for event in events {
                writeln!(file, "{event}").unwrap();
            }
            threads.insert(
                id.to_owned(),
                ManagedThread {
                    id: id.to_owned(),
                    parent_thread_id: parent.map(str::to_owned),
                    path,
                    name: Some("short test title".into()),
                    runtime_status: "notLoaded".into(),
                    ephemeral: false,
                },
            );
        }
        let client = MockClient {
            config: config.clone(),
            threads,
            missing: HashSet::new(),
            deletion: MockDeletion::Success,
            calls: 0,
        };
        (directory, config, client)
    }

    fn prepare(config: &CollectConfig, client: &mut MockClient) -> PreparedSessionDeletion {
        prepare_with_client(config, ROOT, LocalBinding::resolve(config).unwrap(), client).unwrap()
    }

    #[test]
    fn exact_uuid_validation_rejects_names_remote_and_noncanonical_identity() {
        validate_uuid(ROOT).unwrap();
        for value in [
            "session title",
            "remote:11111111-1111-4111-8111-111111111111",
            "../sessions",
            "11111111111141118111111111111111",
            "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
        ] {
            assert!(validate_uuid(value).is_err(), "{value}");
        }
    }

    #[test]
    fn journal_is_durable_before_rpc_and_all_own_members_are_confirmed() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        assert_eq!(prepared.thread_ids.len(), 2);
        assert_eq!(prepared.total_tokens.total_tokens, 20);
        assert!(
            deleted_sessions::load_deleted_sessions(&config)
                .unwrap()
                .is_empty()
        );
        let outcome = execute_with_client(&config, prepared, &mut client).unwrap();
        assert_eq!(outcome.deleted_threads, [ROOT, CHILD]);
        assert_eq!(client.calls, 1);
        let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.status == DeletionStatus::Deleted)
        );
        assert!(
            !serde_json::to_string(&records)
                .unwrap()
                .contains("SECRET PROMPT")
        );
    }

    #[test]
    fn timeout_preserves_all_metrics_as_unknown_and_never_retries() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        client.deletion = MockDeletion::Timeout;
        let error = execute_with_client(&config, prepared, &mut client).unwrap_err();
        assert!(error.to_string().contains("result is unknown"));
        assert_eq!(client.calls, 1);
        let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.status == DeletionStatus::Unknown)
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record.token_usage().total_tokens)
                .sum::<u64>(),
            20
        );
    }

    #[test]
    fn a_child_disappearance_does_not_turn_a_remaining_root_into_success() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        client.deletion = MockDeletion::Partial;
        assert!(execute_with_client(&config, prepared, &mut client).is_err());
        assert_eq!(client.calls, 1);
        let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert_eq!(
            records
                .iter()
                .find(|record| record.thread_id == ROOT)
                .unwrap()
                .status,
            DeletionStatus::Partial
        );
        assert_eq!(
            records
                .iter()
                .find(|record| record.thread_id == CHILD)
                .unwrap()
                .status,
            DeletionStatus::Deleted
        );
    }

    #[test]
    fn changed_log_after_confirmation_prevents_the_mutating_rpc() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        let path = client.threads.get(ROOT).unwrap().path.clone();
        writeln!(
            fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{{}}"
        )
        .unwrap();
        assert!(execute_with_client(&config, prepared, &mut client).is_err());
        assert_eq!(client.calls, 0);
        assert!(
            deleted_sessions::load_deleted_sessions(&config)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn active_and_unknown_runtime_states_are_rejected_before_capture() {
        for status in ["active", "systemError", "newUnknownState"] {
            let (_directory, config, mut client) = fixture();
            client.threads.get_mut(ROOT).unwrap().runtime_status = status.into();
            assert!(
                prepare_with_client(
                    &config,
                    ROOT,
                    LocalBinding::resolve(&config).unwrap(),
                    &mut client
                )
                .is_err()
            );
            assert_eq!(client.calls, 0);
        }
    }

    #[test]
    fn ledger_write_failure_prevents_the_mutating_rpc() {
        let (directory, mut config, mut client) = fixture();
        let bad_root = directory.path().join("not-a-ledger-directory");
        fs::write(&bad_root, "ordinary file").unwrap();
        config.deleted_sessions_dir = Some(bad_root);
        client.config = config.clone();
        let prepared = prepare(&config, &mut client);
        assert!(execute_with_client(&config, prepared, &mut client).is_err());
        assert_eq!(client.calls, 0);
    }

    #[test]
    fn a_crash_after_the_intent_commit_leaves_durable_pending_usage() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        client.deletion = MockDeletion::Crash;
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_with_client(&config, prepared, &mut client)
        }));
        assert!(crash.is_err());
        assert_eq!(client.calls, 1);
        let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.status == DeletionStatus::Pending)
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record.token_usage().total_tokens)
                .sum::<u64>(),
            20
        );
    }

    #[test]
    fn runtime_not_loaded_does_not_hide_an_unfinished_persisted_turn() {
        let (_directory, config, mut client) = fixture();
        let path = client.threads.get(ROOT).unwrap().path.clone();
        writeln!(
            fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{}",
            json!({
                "timestamp":Utc::now()-Duration::seconds(1),"type":"event_msg",
                "payload":{"type":"task_started","turn_id":"new-unfinished-turn","thread_id":ROOT}
            })
        )
        .unwrap();
        assert!(
            prepare_with_client(
                &config,
                ROOT,
                LocalBinding::resolve(&config).unwrap(),
                &mut client
            )
            .is_err()
        );
        assert_eq!(client.calls, 0);
    }

    #[test]
    fn api_identity_cannot_borrow_another_members_log() {
        let (_directory, config, mut client) = fixture();
        let child_path = client.threads.get(CHILD).unwrap().path.clone();
        client.threads.get_mut(ROOT).unwrap().path = child_path;
        assert!(
            prepare_with_client(
                &config,
                ROOT,
                LocalBinding::resolve(&config).unwrap(),
                &mut client
            )
            .is_err()
        );
        assert_eq!(client.calls, 0);
    }

    #[test]
    fn modified_confirmation_summaries_cannot_change_the_sealed_operation() {
        let (_directory, config, mut client) = fixture();
        let prepared = prepare(&config, &mut client);
        let mut changed = prepared.clone();
        changed.thread_ids.retain(|id| id == ROOT);
        assert!(execute_with_client(&config, changed, &mut client).is_err());
        let mut changed = prepared.clone();
        changed.root_thread_id = CHILD.into();
        assert!(execute_with_client(&config, changed, &mut client).is_err());
        let mut changed = prepared.clone();
        changed.total_tokens.total_tokens += 1;
        assert!(execute_with_client(&config, changed, &mut client).is_err());
        let mut changed = prepared;
        changed.captured_at += Duration::seconds(1);
        assert!(execute_with_client(&config, changed, &mut client).is_err());
        assert_eq!(client.calls, 0);
        assert!(
            deleted_sessions::load_deleted_sessions(&config)
                .unwrap()
                .is_empty()
        );
    }
}
