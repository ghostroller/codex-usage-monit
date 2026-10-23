//! Exercises the actual subprocess/RPC adapter as well as capture and storage.
use super::*;
use crate::history::history_namespace;
use crate::rollout::RolloutCache;
use chrono::Duration;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration as StdDuration;

const ROOT: &str = "11111111-1111-4111-8111-111111111111";

struct SharedFixture {
    _server: FixtureServer,
    directory: tempfile::TempDir,
}

impl SharedFixture {
    fn path(&self) -> &Path {
        self.directory.path()
    }
}

struct FixtureServer(Child);

impl Drop for FixtureServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(config: &CollectConfig, physical: &Path) -> FixtureServer {
    let child = Command::new(config.codex_bin.as_ref().unwrap())
        .args(["--serve", physical.to_str().unwrap()])
        .env("CODEX_HOME", &config.codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut server = FixtureServer(child);
    let stdout = server.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    let result = receiver.recv_timeout(StdDuration::from_secs(10));
    if result.is_err() {
        let _ = server.0.kill();
    }
    reader.join().unwrap();
    let line = result.expect("shared fixture server became ready").unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["ready"], true);
    server
}

fn fixture() -> (SharedFixture, CollectConfig) {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("codex-home");
    fs::create_dir(&home).unwrap();
    for name in ["sessions", "archived_sessions"] {
        fs::create_dir(home.join(name)).unwrap();
    }
    let config = CollectConfig {
        codex_home: home,
        codex_bin: Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/mock-codex-session-delete.py"),
        ),
        deleted_sessions_dir: Some(temporary.path().join("ledger")),
        app_server_timeout: std::time::Duration::from_secs(10),
        ..CollectConfig::default()
    };
    let at = Utc::now() - Duration::minutes(20);
    let mut threads = serde_json::Map::new();
    for (index, id) in [
        ROOT,
        "22222222-2222-4222-8222-222222222222",
        "33333333-3333-4333-8333-333333333333",
        "44444444-4444-4444-8444-444444444444",
    ]
    .into_iter()
    .enumerate()
    {
        let archived = index == 3;
        let path = config
            .codex_home
            .join(if archived {
                "archived_sessions"
            } else {
                "sessions"
            })
            .join(format!("rollout-{id}.jsonl"));
        let turn = format!("turn-{index}");
        let source = if index == 0 {
            json!("cli")
        } else {
            json!({"subagent":{"thread_spawn":{"parent_thread_id":ROOT,"depth":1}}})
        };
        let mut file = fs::File::create(&path).unwrap();
        for record in [
            json!({"timestamp":at,"type":"session_meta","payload":{"id":id,"source":source,"cwd":"/fixture/project"}}),
            json!({"timestamp":at+Duration::seconds(1),"type":"event_msg","payload":{"type":"task_started","turn_id":turn,"started_at":(at+Duration::seconds(1)).timestamp()}}),
            json!({"timestamp":at+Duration::seconds(2),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0},"last_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}}),
            json!({"timestamp":at+Duration::seconds(3),"type":"turn_context","payload":{"turn_id":turn,"model":"gpt-6.1-sol","cwd":"/fixture/project"}}),
            json!({"timestamp":at+Duration::seconds(4),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110},"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}),
            json!({"timestamp":at+Duration::seconds(5),"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}}),
        ] {
            writeln!(file, "{record}").unwrap();
        }
        threads.insert(
            id.to_owned(),
            json!({
                "id":id,"parentThreadId":(index>0).then_some(ROOT),
                "path":path,"name":format!("Fixture session {index}"),
                "status":{"type":"notLoaded"},"ephemeral":false,"archived":archived,
            }),
        );
    }
    let canonical_home = fs::canonicalize(&config.codex_home).unwrap();
    let ledger = config
        .deleted_sessions_dir
        .as_ref()
        .unwrap()
        .join(history_namespace(&canonical_home))
        .join("ledger.sqlite3");
    fs::write(
        config.codex_home.join("management-fixture.json"),
        serde_json::to_vec(&json!({
            "root":ROOT,"threads":threads,"ledger":ledger,"tokens":440,
        }))
        .unwrap(),
    )
    .unwrap();
    let server = start_server(&config, &temporary.path().join("shared.sock"));
    (
        SharedFixture {
            directory: temporary,
            _server: server,
        },
        config,
    )
}

fn requests(config: &CollectConfig) -> Vec<Value> {
    fs::read_to_string(config.codex_home.join("management-requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn update_manifest(config: &CollectConfig, update: impl FnOnce(&mut Value)) {
    let path = config.codex_home.join("management-fixture.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    update(&mut manifest);
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
}

fn observer(config: &CollectConfig) -> tungstenite::WebSocket<std::os::unix::net::UnixStream> {
    let path = fs::canonicalize(
        config
            .codex_home
            .join("app-server-control/app-server-control.sock"),
    )
    .unwrap();
    let stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(StdDuration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(StdDuration::from_secs(2)))
        .unwrap();
    let (mut socket, _) = tungstenite::client("ws://localhost/", stream).unwrap();
    for request in [
        json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"desktop-observer","version":"fixture"},"capabilities":{"experimentalApi":true}}}),
        json!({"method":"initialized","params":{}}),
        json!({"id":2,"method":"thread/read","params":{"threadId":ROOT}}),
    ] {
        socket
            .send(tungstenite::Message::Text(request.to_string().into()))
            .unwrap();
        if let Some(id) = request.get("id") {
            let reply: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(&reply["id"], id);
            assert!(reply.get("error").is_none());
        }
    }
    socket
}

#[test]
fn shared_missing_socket_refuses_without_spawning_an_independent_server() {
    let (_temporary, config) = fixture();
    fs::remove_file(
        config
            .codex_home
            .join("app-server-control/app-server-control.sock"),
    )
    .unwrap();
    assert!(prepare_session_delete(&config, ROOT).is_err());
    assert!(requests(&config).is_empty());
    assert!(!config.codex_home.join("proxy-launches.jsonl").exists());
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shared_endpoint_replacement_requires_a_new_confirmation_before_any_ledger_write() {
    let (temporary, config) = fixture();
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    let _replacement = start_server(&config, &temporary.path().join("replacement.sock"));
    let error = execute_session_delete(&config, prepared).unwrap_err();
    assert!(error.to_string().contains("endpoint changed"), "{error:#}");
    assert!(
        requests(&config)
            .iter()
            .all(|request| request["method"] != "thread/delete")
    );
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shared_delete_broadcasts_to_a_second_connection_with_either_reply_order() {
    for events_first in [false, true] {
        let (mut temporary, config) = fixture();
        update_manifest(&config, |manifest| {
            manifest["notifications_before_response"] = json!(events_first)
        });
        let mut desktop = observer(&config);
        let prepared = prepare_session_delete(&config, ROOT).unwrap();
        let expected = prepared.thread_ids.iter().cloned().collect::<HashSet<_>>();
        let outcome = execute_session_delete(&config, prepared).unwrap();
        assert_eq!(
            outcome
                .deleted_threads
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            expected
        );
        let mut observed = HashSet::new();
        while observed.len() < expected.len() {
            let event: Value =
                serde_json::from_str(desktop.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(event["method"], "thread/deleted");
            observed.insert(event["params"]["threadId"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            observed, expected,
            "the desktop's own connection receives all deleted members"
        );
        assert!(
            temporary._server.0.try_wait().unwrap().is_none(),
            "closing monit's proxy must leave the shared service running"
        );
        let mut reopened = SessionManagementClient::connect(&config).unwrap();
        assert!(reopened.persisted_threads(&expected).unwrap().is_empty());
        assert_eq!(
            requests(&config)
                .iter()
                .filter(|request| request["method"] == "thread/delete")
                .count(),
            1
        );
    }
}

#[test]
fn shared_missing_deletion_event_preserves_unknown_usage_without_retrying() {
    let (_temporary, mut config) = fixture();
    config.app_server_timeout = StdDuration::from_millis(500);
    update_manifest(&config, |manifest| {
        manifest["missing_notifications"] = json!([ROOT]);
        manifest["unrelated_notifications"] = json!(["eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee"]);
    });
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    let error = execute_session_delete(&config, prepared).unwrap_err();
    assert!(error.to_string().contains("notification"), "{error:#}");
    assert_eq!(
        requests(&config)
            .iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
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
        440
    );
}

#[test]
fn shared_successful_delete_accepts_not_loaded_only_after_owned_logs_disappear() {
    let (_temporary, config) = fixture();
    update_manifest(&config, |manifest| {
        manifest["missing_not_loaded"] = json!(true)
    });
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert_eq!(
        execute_session_delete(&config, prepared)
            .unwrap()
            .deleted_threads
            .len(),
        4
    );
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .iter()
            .all(|record| record.status == DeletionStatus::Deleted)
    );
}

#[test]
fn shared_uncertain_delete_is_reconciled_without_another_delete_request() {
    let (_temporary, config) = fixture();
    update_manifest(&config, |manifest| {
        manifest["missing_not_loaded"] = json!(true);
        manifest["invalid_delete_reply"] = json!(true);
    });
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert!(execute_session_delete(&config, prepared).is_err());
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .iter()
            .all(|record| record.status == DeletionStatus::Unknown)
    );
    update_manifest(&config, |manifest| {
        let mut extra = (0..40)
            .map(|index| {
                json!({
                    "id": format!("aaaaaaaa-aaaa-4aaa-8aaa-{index:012x}"), "archived": false,
                })
            })
            .collect::<Vec<_>>();
        extra.push(json!({"id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","archived":true}));
        manifest["inventory_extra_threads"] = json!(extra);
    });
    assert_eq!(reconcile_pending_deletions(&config).unwrap().len(), 4);
    assert!(reconcile_pending_deletions(&config).unwrap().is_empty());
    let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
    assert!(
        records
            .iter()
            .all(|record| record.status == DeletionStatus::Deleted)
    );
    assert_eq!(
        records
            .iter()
            .map(|record| record.token_usage().total_tokens)
            .sum::<u64>(),
        440
    );
    let wire = requests(&config);
    assert_eq!(
        wire.iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
    let inventory = wire
        .iter()
        .filter(|request| {
            request["method"] == "thread/list"
                && request["params"].get("ancestorThreadId").is_none()
        })
        .collect::<Vec<_>>();
    assert!(
        inventory
            .iter()
            .any(|request| request["params"]["cursor"] == "33")
    );
    assert!(
        inventory
            .iter()
            .any(|request| request["params"]["archived"] == true)
    );
    assert!(
        inventory
            .iter()
            .all(|request| request["params"]["modelProviders"] == json!([]))
    );
}

#[test]
fn shared_incomplete_inventory_keeps_unknown_receipts_without_retrying_delete() {
    let (_temporary, config) = fixture();
    update_manifest(&config, |manifest| {
        manifest["missing_not_loaded"] = json!(true);
        manifest["invalid_delete_reply"] = json!(true);
    });
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert!(execute_session_delete(&config, prepared).is_err());
    update_manifest(&config, |manifest| {
        manifest["inventory_repeating_cursor"] = json!(true)
    });
    assert!(reconcile_pending_deletions(&config).is_err());
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .iter()
            .all(|record| record.status == DeletionStatus::Unknown)
    );
    assert_eq!(
        requests(&config)
            .iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
}

#[test]
fn shared_delete_saves_usage_before_rpc_and_preserves_restart_totals() {
    let (_temporary, config) = fixture();
    let now = Utc::now();
    let mut before = crate::rollout::scan_rollouts(&config, now).unwrap();
    assert_eq!(
        before
            .calls
            .iter()
            .map(|call| call.tokens.total_tokens)
            .sum::<u64>(),
        440
    );
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert_eq!(prepared.thread_ids.len(), 4);
    assert_eq!(prepared.total_tokens.total_tokens, 440);
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .is_empty()
    );
    let outcome = execute_session_delete(&config, prepared).unwrap();
    assert_eq!(outcome.deleted_threads.len(), 4);
    let retained = deleted_sessions::load_deleted_sessions(&config).unwrap();
    assert!(
        retained
            .iter()
            .all(|record| record.status == DeletionStatus::Deleted)
    );
    let mut reopened = RolloutCache::new();
    let mut after = reopened.scan(&config, Utc::now()).unwrap();
    for dataset in [&mut before, &mut after] {
        dataset.calls.sort_by(|left, right| {
            left.thread_id
                .cmp(&right.thread_id)
                .then_with(|| left.timestamp.cmp(&right.timestamp))
                .then_with(|| left.usage_event_id.cmp(&right.usage_event_id))
        });
    }
    assert_eq!(after.calls, before.calls);
    assert_eq!(after.tasks.len(), 4);
    let history = |dataset: &crate::domain::RolloutDataset| {
        let mut observation = crate::history::HistoryObservation::from_sources_with_tasks_turns_and_interactions_and_coverage(
            now, &dataset.calls, &dataset.tasks, &dataset.turns,
            &dataset.agent_interactions, &[], &[], None,
        );
        for bucket in &mut observation.half_hour_buckets {
            for group in &mut bucket.project_groups {
                group.title = None;
                group.message_preview = None;
            }
        }
        observation
    };
    assert_eq!(
        history(&after),
        history(&before),
        "project, model, turn, pricing and original time buckets must survive deletion"
    );
    let wire = requests(&config);
    assert_eq!(
        wire.iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
    assert!(wire.iter().any(|request| request["method"] == "thread/list" && request["params"]["cursor"] == "1"));
    assert!(
        wire.iter()
            .any(|request| request["method"] == "thread/list"
                && request["params"]["archived"] == true)
    );
    assert!(wire.iter().all(|request| matches!(
        request["method"].as_str(),
        Some("initialize" | "initialized" | "thread/read" | "thread/list" | "thread/delete")
    )));
}

fn link_session_roots(temporary: &Path, config: &CollectConfig) {
    use std::os::unix::fs::symlink;
    for name in ["sessions", "archived_sessions"] {
        let configured = config.codex_home.join(name);
        let external = temporary.join(format!("external-{name}"));
        fs::rename(&configured, &external).unwrap();
        symlink(external, configured).unwrap();
    }
}

#[test]
fn shared_delete_accepts_configured_root_links_and_retains_own_usage() {
    let (temporary, config) = fixture();
    link_session_roots(temporary.path(), &config);
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert_eq!(prepared.total_tokens.total_tokens, 440);
    assert_eq!(
        prepared
            .capture
            .dataset
            .tasks
            .iter()
            .filter(|task| task.archived)
            .count(),
        1
    );
    assert_eq!(prepared.capture.paths().len(), 4);
    assert!(
        prepared
            .capture
            .paths()
            .iter()
            .all(|path| path.starts_with(fs::canonicalize(&config.codex_home).unwrap()))
    );
    let outcome = execute_session_delete(&config, prepared).unwrap();
    assert_eq!(outcome.deleted_threads.len(), 4);
    let records = deleted_sessions::load_deleted_sessions(&config).unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.token_usage().total_tokens)
            .sum::<u64>(),
        440
    );
    assert!(
        records
            .iter()
            .all(|record| record.status == DeletionStatus::Deleted)
    );
    assert_eq!(
        requests(&config)
            .iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
}

#[test]
fn shared_retargeted_root_link_cannot_execute_a_confirmed_deletion() {
    use std::os::unix::fs::symlink;
    for name in ["sessions", "archived_sessions"] {
        let (temporary, config) = fixture();
        link_session_roots(temporary.path(), &config);
        let prepared = prepare_session_delete(&config, ROOT).unwrap();
        let original_target = fs::canonicalize(config.codex_home.join(name)).unwrap();
        let replacement = temporary.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        for entry in fs::read_dir(&original_target).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), replacement.join(entry.file_name())).unwrap();
        }
        let root = config.codex_home.join(name);
        fs::remove_file(&root).unwrap();
        symlink(&replacement, &root).unwrap();
        assert!(execute_session_delete(&config, prepared).is_err());
        assert!(
            requests(&config)
                .iter()
                .all(|request| request["method"] != "thread/delete")
        );
        assert!(
            deleted_sessions::load_deleted_sessions(&config)
                .unwrap()
                .is_empty()
        );
        assert!(fs::read_dir(&original_target).unwrap().next().is_some());
        assert!(fs::read_dir(&replacement).unwrap().next().is_some());
    }
}

#[test]
fn shared_mismatched_identity_cannot_authorize_deletion() {
    let (_temporary, config) = fixture();
    let path = config.codex_home.join("management-fixture.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["mismatched_identity"] = json!(true);
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(prepare_session_delete(&config, ROOT).is_err());
    assert!(
        deleted_sessions::load_deleted_sessions(&config)
            .unwrap()
            .is_empty()
    );
    assert!(
        requests(&config)
            .iter()
            .all(|request| request["method"] != "thread/delete")
    );
}

#[test]
fn shared_invalid_delete_reply_is_not_retried_and_usage_remains_saved() {
    let (_temporary, config) = fixture();
    let path = config.codex_home.join("management-fixture.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["invalid_delete_reply"] = json!(true);
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let prepared = prepare_session_delete(&config, ROOT).unwrap();
    assert!(execute_session_delete(&config, prepared).is_err());
    assert_eq!(
        requests(&config)
            .iter()
            .filter(|request| request["method"] == "thread/delete")
            .count(),
        1
    );
    let saved = deleted_sessions::load_deleted_sessions(&config).unwrap();
    assert_eq!(
        saved
            .iter()
            .map(|record| record.token_usage().total_tokens)
            .sum::<u64>(),
        440
    );
    assert!(
        saved
            .iter()
            .all(|record| record.status == DeletionStatus::Unknown)
    );
}
