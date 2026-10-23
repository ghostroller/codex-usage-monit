//! Exercises the actual subprocess/RPC adapter as well as capture and storage.
use super::*;
use crate::history::history_namespace;
use crate::rollout::RolloutCache;
use chrono::Duration;
use serde_json::{Value, json};
use std::io::Write;

const ROOT: &str = "11111111-1111-4111-8111-111111111111";

fn fixture() -> (tempfile::TempDir, CollectConfig) {
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
    (temporary, config)
}

fn requests(config: &CollectConfig) -> Vec<Value> {
    fs::read_to_string(config.codex_home.join("management-requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn stdio_delete_saves_usage_before_rpc_and_preserves_restart_totals() {
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

#[test]
fn stdio_mismatched_identity_cannot_authorize_deletion() {
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
fn stdio_invalid_delete_reply_is_not_retried_and_usage_remains_saved() {
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
            .all(|record| record.status == DeletionStatus::Deleted)
    );
}
