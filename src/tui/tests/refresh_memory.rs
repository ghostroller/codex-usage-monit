use std::cell::Cell;
use std::fs;

use serde_json::json;

use super::*;

fn local_rollout_fixture(directory: &Path) -> (CollectConfig, PathBuf) {
    let sessions = directory.join("sessions");
    fs::create_dir(&sessions).unwrap();
    let rollout = sessions.join("rollout-refresh-memory.jsonl");
    let timestamp = (Utc::now() - ChronoDuration::minutes(2)).to_rfc3339();
    let records = [
        json!({"timestamp": timestamp, "type": "session_meta", "payload": {
            "id": RESUMABLE_THREAD_ID, "cwd": "/refresh-memory", "source": "cli"
        }}),
        json!({"timestamp": timestamp, "type": "event_msg", "payload": {
            "type": "task_started", "turn_id": "turn-memory"
        }}),
        json!({"timestamp": timestamp, "type": "turn_context", "payload": {
            "turn_id": "turn-memory", "model": "gpt-5.6-sol"
        }}),
        json!({"timestamp": timestamp, "type": "token_usage_record", "payload": {
            "thread_id": RESUMABLE_THREAD_ID, "turn_id": "turn-memory",
            "response_id": "response-memory", "usage": {
                "input_tokens": 80, "output_tokens": 20, "total_tokens": 100
            }
        }}),
        json!({"timestamp": timestamp, "type": "event_msg", "payload": {
            "type": "task_complete", "turn_id": "turn-memory"
        }}),
    ];
    let mut contents = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    contents.push('\n');
    fs::write(&rollout, contents).unwrap();
    (
        CollectConfig {
            codex_home: directory.to_path_buf(),
            offline: true,
            ..CollectConfig::default()
        },
        rollout,
    )
}

#[test]
fn local_refresh_collects_rollouts_without_copying_a_cached_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (config, _) = local_rollout_fixture(directory.path());
    let cache = Arc::new(Mutex::new(RolloutCache::new()));

    for force_materialization in [false, true] {
        let inputs = RefreshCollectionInputs::prepare(
            false,
            AccountSnapshot::default(),
            force_materialization,
            || panic!("a local refresh must not copy the previous snapshot"),
        );
        let result = inputs.collect(&config, &cache).unwrap();

        assert_eq!(result.snapshot.tasks.len(), 1);
        assert_eq!(result.snapshot.tasks[0].thread_id, RESUMABLE_THREAD_ID);
        assert_eq!(result.snapshot.tasks[0].token_usage.total_tokens, 100);
        assert_eq!(result.snapshot.turns.len(), 1);
        assert_eq!(result.snapshot.turns[0].turn_id, "turn-memory");
        assert!(!result.history_observation.half_hour_buckets.is_empty());
        if force_materialization {
            let refresh = cache.lock().unwrap().last_refresh();
            assert_eq!(refresh.reused_files, 1);
            assert_eq!(refresh.reparsed_files, 0);
        }
    }
}

#[test]
fn account_refresh_copies_local_projection_once_and_does_not_rescan_rollouts() {
    let directory = tempfile::tempdir().unwrap();
    let (config, rollout) = local_rollout_fixture(directory.path());
    let cache = Arc::new(Mutex::new(RolloutCache::new()));
    let mut local =
        collect_snapshot_cached(&config, None, false, &mut cache.lock().unwrap()).snapshot;
    local.warnings.push("cached local warning".to_owned());
    let previous_stats = local.stats.clone();
    let previous_metrics = cache.lock().unwrap().metrics();
    // A mistaken rollout scan would replace the cached task/turn projection.
    fs::write(rollout, "{}\n").unwrap();
    let now = Utc::now();
    let account = AccountSnapshot {
        limits: vec![LimitBucket {
            limit_id: "codex".to_owned(),
            limit_name: None,
            plan_type: None,
            primary: Some(LimitWindow::new(
                25.0,
                Some(300),
                Some(now + ChronoDuration::hours(1)),
            )),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }],
        ..AccountSnapshot::default()
    };
    let copies = Cell::new(0);
    let inputs = RefreshCollectionInputs::prepare(true, account, true, || {
        copies.set(copies.get() + 1);
        local.clone()
    });
    let result = inputs.collect(&config, &cache).unwrap();

    assert_eq!(copies.get(), 1);
    assert_eq!(result.snapshot.tasks.len(), 1);
    assert_eq!(result.snapshot.tasks[0].thread_id, RESUMABLE_THREAD_ID);
    assert_eq!(result.snapshot.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(result.snapshot.turns.len(), 1);
    assert_eq!(result.snapshot.turns[0].turn_id, "turn-memory");
    assert_eq!(result.snapshot.turns[0].token_usage.total_tokens, 100);
    assert_eq!(result.snapshot.stats, previous_stats);
    assert!(
        result
            .snapshot
            .warnings
            .contains(&"cached local warning".to_owned())
    );
    assert_eq!(
        result.snapshot.limits[0]
            .primary
            .as_ref()
            .unwrap()
            .used_percent,
        25.0
    );
    assert!(!result.history_observation.quota_points.is_empty());
    assert!(result.history_observation.half_hour_buckets.is_empty());
    assert!(result.history_observation.weekly_local_points.is_empty());
    assert_eq!(result.local_session_digests.digest_count(), 0);
    assert_eq!(cache.lock().unwrap().metrics(), previous_metrics);
}
