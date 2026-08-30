use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use codex_usage_monit::config::CollectConfig;
use codex_usage_monit::domain::{RolloutDataset, TaskStatus, TurnStatus};
use codex_usage_monit::rollout::{RolloutCache, scan_rollouts};
use codex_usage_monit::snapshot::{collect_snapshot_cached, collect_snapshot_cached_if_changed};
use codex_usage_monit::startup::StartupTrace;
use serde_json::{Value, json};
use tempfile::TempDir;

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn usage(total: u64) -> Value {
    json!({
        "input_tokens": total.saturating_sub(2),
        "cached_input_tokens": 0,
        "output_tokens": total.min(2),
        "reasoning_output_tokens": 0,
        "total_tokens": total
    })
}

fn write_jsonl(path: &std::path::Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(path).unwrap();
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
    }
}

fn append_jsonl(path: &std::path::Path, records: &[Value]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
    }
    file.flush().unwrap();
}

fn config(home: &std::path::Path) -> CollectConfig {
    CollectConfig {
        codex_home: home.to_owned(),
        ..CollectConfig::default()
    }
}

fn persistent_config(home: &Path, cache_root: &Path) -> CollectConfig {
    let mut config = config(home);
    config.rollout_cache_dir = Some(cache_root.to_owned());
    config
}

fn cache_entries(cache_root: &Path) -> Vec<PathBuf> {
    let mut entries = walkdir::WalkDir::new(cache_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn assert_dataset_eq(left: &RolloutDataset, right: &RolloutDataset) {
    assert_eq!(left.tasks, right.tasks);
    assert_eq!(left.turns, right.turns);
    assert_eq!(left.agent_interactions, right.agent_interactions);
    assert_eq!(left.calls, right.calls);
    assert_eq!(left.rate_observations, right.rate_observations);
    assert_eq!(left.stats, right.stats);
    assert_eq!(left.warnings, right.warnings);
}

#[test]
fn cached_refresh_rebuilds_truncated_replay_warnings_in_file_order() {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let now = Utc::now();
    let base = now - chrono::Duration::hours(1);
    let write_resets = |path: &Path, thread_id: &str, at: DateTime<Utc>, reset_count: usize| {
        let mut records = vec![
            json!({
                "timestamp": timestamp(at),
                "type": "session_meta",
                "payload": {"id": thread_id, "timestamp": timestamp(at)}
            }),
            json!({
                "timestamp": timestamp(at),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(1_000)}
                }
            }),
        ];
        for reset in 0..reset_count {
            let at = at + chrono::Duration::milliseconds((reset + 1) as i64);
            records.push(json!({
                "timestamp": timestamp(at),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(999 - reset as u64)}
                }
            }));
        }
        write_jsonl(path, &records);
    };

    let mut first_path = None;
    for index in 0..127 {
        let path = sessions.join(format!("rollout-{index:03}.jsonl"));
        if index == 0 {
            first_path = Some(path.clone());
        }
        write_resets(
            &path,
            &format!("thread-{index:03}"),
            base + chrono::Duration::seconds(index),
            if index == 0 { 2 } else { 1 },
        );
    }

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let overflowed = cache.scan(&scan_config, now).unwrap();
    assert_eq!(overflowed.stats.ambiguous_token_resets, 128);
    assert!(
        overflowed
            .warnings
            .last()
            .is_some_and(|warning| warning.starts_with("suppressed 1 additional"))
    );

    write_resets(first_path.as_ref().unwrap(), "thread-000", base, 0);
    let cached = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    let fresh = scan_rollouts(&scan_config, now + chrono::Duration::seconds(1)).unwrap();

    assert!(cache.last_refresh().full_rebuild);
    assert_eq!(cached.stats.ambiguous_token_resets, 126);
    assert!(
        cached
            .warnings
            .iter()
            .all(|warning| !warning.starts_with("suppressed "))
    );
    assert_dataset_eq(&cached, &fresh);
}

#[test]
fn cached_snapshot_skips_derive_until_rollout_changes() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp
        .path()
        .join("sessions/rollout-snapshot-fast-path.jsonl");
    write_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "snapshot-fast-path-thread", "timestamp": timestamp(now)}
        })],
    );
    let mut scan_config = config(temp.path());
    scan_config.offline = true;
    let mut cache = RolloutCache::new();

    let first = collect_snapshot_cached(&scan_config, None, false, &mut cache);
    assert_eq!(first.snapshot.tasks.len(), 1);
    assert!(
        collect_snapshot_cached_if_changed(&scan_config, Some(first.account.clone()), &mut cache,)
            .is_none()
    );

    append_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(Utc::now()),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "snapshot-fast-path-turn"}
        })],
    );
    let changed = collect_snapshot_cached_if_changed(&scan_config, Some(first.account), &mut cache)
        .expect("an appended rollout record must produce a new snapshot");
    assert_eq!(changed.snapshot.turns.len(), 1);
}

#[test]
fn tail_cache_joins_an_agent_activity_appended_after_its_function_call() {
    let temp = TempDir::new().unwrap();
    let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
    let parent_path = temp.path().join("sessions/rollout-agent-parent.jsonl");
    write_jsonl(
        &parent_path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "agent-parent", "source": "vscode"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "agent-parent-turn"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "spawn_agent",
                    "call_id": "agent-call",
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "agent-parent-turn"
                    }
                }
            }),
        ],
    );
    write_jsonl(
        &temp.path().join("sessions/rollout-agent-child.jsonl"),
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {
                "id": "agent-child",
                "thread_source": "subagent",
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": "agent-parent",
                    "agent_role": null
                }}}
            }
        })],
    );
    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();

    let before = cache.scan(&scan_config, now).unwrap();
    assert!(before.agent_interactions.is_empty());

    append_jsonl(
        &parent_path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "kind": "started",
                "event_id": "agent-call",
                "agent_thread_id": "agent-child"
            }
        })],
    );
    let cached = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(cache.last_refresh().tail_parsed_files, 1);
    assert_eq!(cache.last_refresh().incrementally_reduced_threads, 1);
    assert_eq!(cached.agent_interactions.len(), 1);
    assert_eq!(
        cached.agent_interactions[0].parent_turn_id,
        "agent-parent-turn"
    );
    assert_eq!(cached.agent_interactions[0].child_thread_id, "agent-child");

    let fresh = scan_rollouts(&scan_config, now + chrono::Duration::seconds(2)).unwrap();
    assert_dataset_eq(&cached, &fresh);
}

#[test]
fn persistent_cache_is_reused_by_a_new_instance_and_recomputes_freshness() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    write_jsonl(
        &temp.path().join("sessions/rollout-persistent-active.jsonl"),
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "persistent-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "persistent-turn"}
            }),
        ],
    );
    let mut scan_config = persistent_config(temp.path(), &cache_root);
    scan_config.active_grace = Duration::from_secs(30);

    let first = RolloutCache::new().scan(&scan_config, now).unwrap();
    assert_eq!(first.tasks[0].status, TaskStatus::Running);
    assert_eq!(cache_entries(&cache_root).len(), 1);

    let mut reopened = RolloutCache::new();
    let second = reopened
        .scan(&scan_config, now + chrono::Duration::minutes(2))
        .unwrap();
    assert_eq!(second.tasks[0].status, TaskStatus::Stale);
    assert_eq!(reopened.last_refresh().disk_reused_files, 1);
    assert_eq!(reopened.last_refresh().reused_files, 1);
    assert_eq!(reopened.last_refresh().reparsed_files, 0);
    assert!(reopened.last_refresh().rebuilt);
}

#[test]
fn reopened_cache_reparses_only_a_changed_file_and_matches_a_fresh_scan() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    let first_path = temp.path().join("sessions/rollout-persistent-a.jsonl");
    let changed_path = temp.path().join("sessions/rollout-persistent-b.jsonl");
    write_jsonl(
        &first_path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "persistent-shared"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-a"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(10)}}}),
        ],
    );
    write_jsonl(
        &changed_path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "persistent-shared"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-b"}}),
            json!({"timestamp": timestamp(now), "type": "turn_context", "payload": {"turn_id": "turn-b", "model": "gpt-persistent"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(15)}}}),
        ],
    );
    let scan_config = persistent_config(temp.path(), &cache_root);
    RolloutCache::new().scan(&scan_config, now).unwrap();

    append_jsonl(
        &changed_path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(20)}}
        })],
    );
    let mut reopened = RolloutCache::new();
    let cached = reopened
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(reopened.last_refresh().disk_reused_files, 1);
    assert_eq!(reopened.last_refresh().reused_files, 1);
    assert_eq!(reopened.last_refresh().reparsed_files, 1);
    assert_eq!(reopened.last_refresh().disk_written_files, 1);

    let mut uncached_config = scan_config.clone();
    uncached_config.rollout_cache_dir = None;
    let fresh = scan_rollouts(&uncached_config, now + chrono::Duration::seconds(2)).unwrap();
    assert_dataset_eq(&cached, &fresh);

    fs::remove_file(&changed_path).unwrap();
    let mut after_delete = RolloutCache::new();
    let deleted = after_delete
        .scan(&scan_config, now + chrono::Duration::seconds(3))
        .unwrap();
    assert_eq!(deleted.stats.scanned_files, 1);
    assert!(deleted.turns.iter().all(|turn| turn.turn_id != "turn-b"));
    assert_eq!(after_delete.last_refresh().disk_reused_files, 1);
    assert_eq!(after_delete.last_refresh().reparsed_files, 0);
}

#[test]
fn changed_in_memory_file_skips_the_stale_disk_entry() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-active-cache.jsonl");
    write_jsonl(
        &path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "active-cache-thread"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "active-cache-turn"}}),
        ],
    );
    let scan_config = persistent_config(temp.path(), &cache_root);
    let mut cache = RolloutCache::new();
    cache.scan(&scan_config, now).unwrap();
    let entry = cache_entries(&cache_root).pop().unwrap();
    fs::write(entry, b"{corrupt stale entry").unwrap();

    append_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": "active-cache-turn"}
        })],
    );
    let dataset = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();

    assert_eq!(dataset.turns[0].status, TurnStatus::Completed);
    assert_eq!(cache.last_refresh().disk_corrupt_files, 0);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
}

#[test]
fn warm_owner_probe_cache_cannot_bypass_the_omitted_candidate_cap() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let old_modified = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    for index in 0..64 {
        let path = temp
            .path()
            .join(format!("sessions/old/rollout-ambiguous-{index}.jsonl"));
        write_jsonl(
            &path,
            &[json!({
                "type": "session_meta",
                "payload": {"id": format!("unrelated-{index}")}
            })],
        );
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old_modified)
            .unwrap();
    }
    write_jsonl(
        &temp
            .path()
            .join("sessions/current/rollout-selected-owner.jsonl"),
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "selected-owner"}
            }),
            json!({
                "timestamp": timestamp(now + chrono::Duration::seconds(1)),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {
                    "total_token_usage": usage(10)
                }}
            }),
        ],
    );
    let mut scan_config = config(temp.path());
    scan_config.lookback_days = 1;
    let mut cache = RolloutCache::new();

    let first = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(first.calls.len(), 1);
    assert_eq!(first.stats.ambiguous_token_resets, 0);

    let overflow = temp
        .path()
        .join("sessions/old/rollout-ambiguous-overflow.jsonl");
    write_jsonl(
        &overflow,
        &[json!({
            "type": "session_meta",
            "payload": {"id": "unrelated-overflow"}
        })],
    );
    File::options()
        .write(true)
        .open(overflow)
        .unwrap()
        .set_modified(old_modified)
        .unwrap();

    let overflowed = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert!(overflowed.calls.is_empty());
    assert_eq!(overflowed.tasks[0].token_usage.total_tokens, 0);
    assert_eq!(overflowed.stats.ambiguous_token_resets, 1);

    let warm = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert!(cache.last_refresh().discovery_cache_hit);
    assert!(warm.calls.is_empty());
    assert_eq!(warm.stats.ambiguous_token_resets, 1);
}

#[test]
fn successful_persistent_write_debounces_a_changed_active_file() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-write-debounce.jsonl");
    write_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "write-debounce-thread"}
        })],
    );
    let scan_config = persistent_config(temp.path(), &cache_root);
    let mut cache = RolloutCache::new();
    cache.scan(&scan_config, now).unwrap();
    assert_eq!(cache.last_refresh().disk_written_files, 1);

    append_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "write-debounce-turn"}
        })],
    );
    cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();

    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cache.last_refresh().disk_written_files, 0);
    assert_eq!(cache.last_refresh().disk_deferred_files, 1);
    assert!(cache.last_refresh().disk_write_retry_ms > 0);
}

#[test]
fn corrupt_and_future_persistent_entries_fall_back_and_are_repaired() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    write_jsonl(
        &temp.path().join("sessions/rollout-cache-repair.jsonl"),
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "repair-thread"}
        })],
    );
    let scan_config = persistent_config(temp.path(), &cache_root);
    RolloutCache::new().scan(&scan_config, now).unwrap();
    let entry = cache_entries(&cache_root).pop().unwrap();

    fs::write(&entry, b"{truncated").unwrap();
    let mut corrupt = RolloutCache::new();
    let repaired = corrupt.scan(&scan_config, now).unwrap();
    assert_eq!(repaired.tasks[0].thread_id, "repair-thread");
    assert_eq!(corrupt.last_refresh().disk_corrupt_files, 1);
    assert_eq!(corrupt.last_refresh().reparsed_files, 1);
    assert_eq!(corrupt.last_refresh().disk_written_files, 1);

    let mut future: Value = serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
    future["formatVersion"] = json!(u32::MAX);
    fs::write(&entry, serde_json::to_vec(&future).unwrap()).unwrap();
    let mut incompatible = RolloutCache::new();
    incompatible.scan(&scan_config, now).unwrap();
    assert_eq!(incompatible.last_refresh().disk_misses, 1);
    assert_eq!(incompatible.last_refresh().disk_corrupt_files, 0);
    assert_eq!(incompatible.last_refresh().reparsed_files, 1);

    let mut stale_parser: Value = serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
    stale_parser["parserRevision"] = json!(u32::MAX);
    fs::write(&entry, serde_json::to_vec(&stale_parser).unwrap()).unwrap();
    let mut reparsed = RolloutCache::new();
    reparsed.scan(&scan_config, now).unwrap();
    assert_eq!(reparsed.last_refresh().disk_misses, 1);
    assert_eq!(reparsed.last_refresh().disk_corrupt_files, 0);
    assert_eq!(reparsed.last_refresh().reparsed_files, 1);
}

#[test]
fn redacted_persistent_cache_is_isolated_and_contains_no_message_preview() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    let private_message = "private persistent cache message";
    write_jsonl(
        &temp.path().join("sessions/rollout-cache-redaction.jsonl"),
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "redaction-thread"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "redaction-turn"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": private_message}],
                    "internal_chat_message_metadata_passthrough": {
                        "turn_id": "redaction-turn"
                    }
                }
            }),
        ],
    );
    let visible_config = persistent_config(temp.path(), &cache_root);
    RolloutCache::new().scan(&visible_config, now).unwrap();
    let visible_entries = cache_entries(&cache_root)
        .into_iter()
        .collect::<std::collections::HashSet<_>>();

    let mut redacted_config = visible_config.clone();
    redacted_config.redact_content = true;
    let mut redacted_cache = RolloutCache::new();
    let redacted = redacted_cache.scan(&redacted_config, now).unwrap();
    assert_eq!(redacted.tasks[0].title, "[redacted]");
    assert_eq!(redacted_cache.last_refresh().disk_reused_files, 0);
    assert_eq!(redacted_cache.last_refresh().reparsed_files, 1);

    let redacted_entry = cache_entries(&cache_root)
        .into_iter()
        .find(|path| !visible_entries.contains(path))
        .unwrap();
    let contents = fs::read_to_string(redacted_entry).unwrap();
    assert!(!contents.contains(private_message));
}

#[test]
fn persistent_write_failure_does_not_fail_the_rollout_snapshot() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    write_jsonl(
        &temp
            .path()
            .join("sessions/rollout-cache-write-failure.jsonl"),
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "write-failure-thread"}
        })],
    );
    let unusable_root = temp.path().join("cache-file");
    fs::write(&unusable_root, b"not a directory").unwrap();
    let scan_config = persistent_config(temp.path(), &unusable_root);
    let mut cache = RolloutCache::new();

    let dataset = cache.scan(&scan_config, now).unwrap();

    assert_eq!(dataset.tasks[0].thread_id, "write-failure-thread");
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cache.last_refresh().disk_write_failures, 1);

    cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(cache.last_refresh().disk_write_failures, 0);
    assert_eq!(cache.last_refresh().disk_deferred_files, 1);
    assert!(cache.last_refresh().disk_write_retry_ms > 0);
}

#[test]
fn persistent_cache_reuses_the_intersection_when_file_selection_expands() {
    let temp = TempDir::new().unwrap();
    let cache_root = temp.path().join("cache");
    let now = Utc::now();
    for index in 0..2 {
        write_jsonl(
            &temp
                .path()
                .join(format!("sessions/rollout-selection-{index}.jsonl")),
            &[json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": format!("selection-thread-{index}")}
            })],
        );
    }
    let mut narrow = persistent_config(temp.path(), &cache_root);
    narrow.max_files = 1;
    RolloutCache::new().scan(&narrow, now).unwrap();

    let mut expanded = narrow.clone();
    expanded.max_files = 2;
    let mut cache = RolloutCache::new();
    let dataset = cache.scan(&expanded, now).unwrap();

    assert_eq!(dataset.tasks.len(), 2);
    assert_eq!(cache.last_refresh().disk_reused_files, 1);
    assert_eq!(cache.last_refresh().reused_files, 1);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
}

#[test]
fn unchanged_scan_reuses_files_and_recomputes_freshness() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-active.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "active-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "active-turn"}
            }),
        ],
    );

    let mut scan_config = config(temp.path());
    scan_config.active_grace = Duration::from_secs(30);
    let mut cache = RolloutCache::new();
    let first = cache.scan(&scan_config, now).unwrap();
    assert_eq!(first.tasks[0].status, TaskStatus::Running);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert!(cache.last_refresh().rebuilt);

    let second = cache
        .scan(&scan_config, now + chrono::Duration::minutes(2))
        .unwrap();
    assert_eq!(second.tasks[0].status, TaskStatus::Stale);
    assert_eq!(cache.last_refresh().reused_files, 1);
    assert_eq!(cache.last_refresh().reparsed_files, 0);
    assert!(!cache.last_refresh().rebuilt);
}

#[test]
fn scan_if_changed_skips_warm_materialization_until_freshness_crosses() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-conditional.jsonl");
    write_jsonl(
        &path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "conditional-thread"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "conditional-turn"}}),
        ],
    );
    let mut scan_config = config(temp.path());
    scan_config.active_grace = Duration::from_secs(30);
    let mut cache = RolloutCache::new();

    let initial = cache.scan_if_changed(&scan_config, now).unwrap().unwrap();
    assert_eq!(initial.tasks[0].status, TaskStatus::Running);
    assert!(
        cache
            .scan_if_changed(&scan_config, now + chrono::Duration::seconds(10))
            .unwrap()
            .is_none()
    );

    let stale = cache
        .scan_if_changed(&scan_config, now + chrono::Duration::seconds(31))
        .unwrap()
        .unwrap();
    assert_eq!(stale.tasks[0].status, TaskStatus::Stale);
    assert!(
        cache
            .scan_if_changed(&scan_config, now + chrono::Duration::seconds(32))
            .unwrap()
            .is_none()
    );

    append_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(33)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": "conditional-turn"}
        })],
    );
    let completed = cache
        .scan_if_changed(&scan_config, now + chrono::Duration::seconds(33))
        .unwrap()
        .unwrap();
    assert_eq!(completed.tasks[0].status, TaskStatus::Completed);
    assert_eq!(cache.last_refresh().tail_parsed_files, 1);
    assert_eq!(cache.last_refresh().materialize_us, 0);
}

#[test]
fn tail_parse_falls_back_when_the_cached_prefix_was_rewritten() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-rewritten.jsonl");
    write_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "before-rewrite"}
        })],
    );
    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    cache.scan(&scan_config, now).unwrap();

    write_jsonl(
        &path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "after-rewrite-with-longer-id"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "rewritten-turn"}}),
        ],
    );
    let rewritten = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();

    assert_eq!(cache.last_refresh().tail_parsed_files, 0);
    assert_eq!(cache.last_refresh().full_parsed_files, 1);
    assert_eq!(rewritten.tasks[0].thread_id, "after-rewrite-with-longer-id");
}

#[test]
fn cached_scan_refreshes_session_titles_without_reparsing_rollouts() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-renamed.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "renamed-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fallback message title"}
            }),
        ],
    );
    let index_path = temp.path().join("session_index.jsonl");
    write_jsonl(
        &index_path,
        &[json!({
            "id": "renamed-thread",
            "thread_name": "Initial session title",
            "updated_at": timestamp(now)
        })],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let first = cache.scan(&scan_config, now).unwrap();
    assert_eq!(first.tasks[0].title, "Initial session title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
    assert!(!cache.last_refresh().session_index_reused);

    let unchanged = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(unchanged.tasks[0].title, "Initial session title");
    assert_eq!(cache.last_refresh().session_index_reads, 0);
    assert!(cache.last_refresh().session_index_reused);

    write_jsonl(
        &index_path,
        &[
            json!({
                "id": "renamed-thread",
                "thread_name": "Initial session title",
                "updated_at": timestamp(now)
            }),
            json!({
                "id": "renamed-thread",
                "thread_name": "Renamed while TUI is running",
                "updated_at": timestamp(now + chrono::Duration::seconds(1))
            }),
        ],
    );
    let second = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();

    assert_eq!(second.tasks[0].title, "Renamed while TUI is running");
    assert_eq!(cache.last_refresh().reused_files, 1);
    assert_eq!(cache.last_refresh().reparsed_files, 0);
    assert!(!cache.last_refresh().rebuilt);
    assert_eq!(cache.last_refresh().session_index_reads, 1);
    assert!(!cache.last_refresh().session_index_reused);
}

#[test]
fn cached_session_titles_follow_index_deletion_and_recreation() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let rollout_path = temp.path().join("sessions/rollout-index-lifecycle.jsonl");
    write_jsonl(
        &rollout_path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "lifecycle-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fallback lifecycle title"}
            }),
        ],
    );
    let index_path = temp.path().join("session_index.jsonl");
    write_jsonl(
        &index_path,
        &[json!({
            "id": "lifecycle-thread",
            "thread_name": "Indexed lifecycle title",
            "updated_at": timestamp(now)
        })],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let indexed = cache.scan(&scan_config, now).unwrap();
    assert_eq!(indexed.tasks[0].title, "Indexed lifecycle title");

    fs::remove_file(&index_path).unwrap();
    let deleted = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(deleted.tasks[0].title, "Fallback lifecycle title");
    assert_eq!(cache.last_refresh().session_index_reads, 0);
    assert!(!cache.last_refresh().session_index_reused);

    cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(cache.last_refresh().session_index_reads, 0);
    assert!(cache.last_refresh().session_index_reused);

    write_jsonl(
        &index_path,
        &[json!({
            "id": "lifecycle-thread",
            "thread_name": "Recreated lifecycle title",
            "updated_at": timestamp(now + chrono::Duration::seconds(3))
        })],
    );
    let recreated = cache
        .scan(&scan_config, now + chrono::Duration::seconds(3))
        .unwrap();
    assert_eq!(recreated.tasks[0].title, "Recreated lifecycle title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
}

#[test]
fn session_title_cache_is_scoped_to_codex_home_and_redaction() {
    let first_home = TempDir::new().unwrap();
    let second_home = TempDir::new().unwrap();
    let now = Utc::now();
    for (home, title) in [
        (&first_home, "First home title"),
        (&second_home, "Second home title"),
    ] {
        write_jsonl(
            &home.path().join("sessions/rollout-key.jsonl"),
            &[
                json!({
                    "timestamp": timestamp(now),
                    "type": "session_meta",
                    "payload": {"id": "key-thread", "timestamp": timestamp(now)}
                }),
                json!({
                    "timestamp": timestamp(now),
                    "type": "event_msg",
                    "payload": {"type": "user_message", "message": "Fallback key title"}
                }),
            ],
        );
        write_jsonl(
            &home.path().join("session_index.jsonl"),
            &[json!({
                "id": "key-thread",
                "thread_name": title,
                "updated_at": timestamp(now)
            })],
        );
    }

    let mut cache = RolloutCache::new();
    let first_config = config(first_home.path());
    let first = cache.scan(&first_config, now).unwrap();
    assert_eq!(first.tasks[0].title, "First home title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);

    let second_config = config(second_home.path());
    let second = cache.scan(&second_config, now).unwrap();
    assert_eq!(second.tasks[0].title, "Second home title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);

    let mut redacted = second_config.clone();
    redacted.redact_content = true;
    let hidden = cache.scan(&redacted, now).unwrap();
    assert_eq!(hidden.tasks[0].title, "[redacted]");
    assert_eq!(cache.last_refresh().session_index_reads, 0);
    assert!(!cache.last_refresh().session_index_reused);

    fs::remove_file(second_home.path().join("session_index.jsonl")).unwrap();
    let visible_again = cache.scan(&second_config, now).unwrap();
    assert_eq!(visible_again.tasks[0].title, "Fallback key title");
    assert_eq!(cache.last_refresh().session_index_reads, 0);
}

#[test]
fn cached_session_titles_recover_when_a_partial_line_is_completed() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    write_jsonl(
        &temp.path().join("sessions/rollout-partial-title.jsonl"),
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "partial-thread", "timestamp": timestamp(now)}
        })],
    );
    let index_path = temp.path().join("session_index.jsonl");
    write_jsonl(
        &index_path,
        &[json!({
            "id": "partial-thread",
            "thread_name": "Complete title",
            "updated_at": timestamp(now)
        })],
    );
    let mut index = OpenOptions::new().append(true).open(&index_path).unwrap();
    write!(
        index,
        "{{\"id\":\"partial-thread\",\"thread_name\":\"Recovered title"
    )
    .unwrap();
    index.flush().unwrap();

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let partial = cache.scan(&scan_config, now).unwrap();
    assert_eq!(partial.tasks[0].title, "Complete title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);

    writeln!(
        index,
        "\",\"updated_at\":\"{}\"}}",
        timestamp(now + chrono::Duration::seconds(1))
    )
    .unwrap();
    index.flush().unwrap();
    let recovered = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(recovered.tasks[0].title, "Recovered title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
}

#[test]
fn session_index_read_errors_are_retried() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    write_jsonl(
        &temp.path().join("sessions/rollout-index-error.jsonl"),
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "error-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fallback after index error"}
            }),
        ],
    );
    let index_path = temp.path().join("session_index.jsonl");
    write_jsonl(
        &index_path,
        &[json!({
            "id": "error-thread",
            "thread_name": "Previously cached title",
            "updated_at": timestamp(now)
        })],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let cached = cache.scan(&scan_config, now).unwrap();
    assert_eq!(cached.tasks[0].title, "Previously cached title");
    assert_eq!(cache.last_refresh().session_index_reads, 1);

    fs::remove_file(&index_path).unwrap();
    fs::create_dir(&index_path).unwrap();
    let first = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(first.tasks[0].title, "Fallback after index error");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
    assert!(
        first
            .warnings
            .iter()
            .any(|warning| warning.contains("session title index unavailable"))
    );

    let second = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(second.tasks[0].title, "Fallback after index error");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
    assert!(!cache.last_refresh().session_index_reused);

    fs::remove_dir(&index_path).unwrap();
    write_jsonl(
        &index_path,
        &[json!({
            "id": "error-thread",
            "thread_name": "Recovered after read error",
            "updated_at": timestamp(now + chrono::Duration::seconds(3))
        })],
    );
    let recovered = cache
        .scan(&scan_config, now + chrono::Duration::seconds(3))
        .unwrap();
    assert_eq!(recovered.tasks[0].title, "Recovered after read error");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
}

#[test]
fn changing_one_file_reuses_others_and_matches_a_fresh_scan() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let first_at = now - chrono::Duration::minutes(2);
    let second_at = now - chrono::Duration::minutes(1);
    let first_path = temp.path().join("sessions/rollout-a.jsonl");
    let changed_path = temp.path().join("sessions/rollout-b.jsonl");
    let unrelated_path = temp.path().join("sessions/rollout-c.jsonl");

    write_jsonl(
        &first_path,
        &[
            json!({"timestamp": timestamp(first_at), "type": "session_meta", "payload": {"id": "shared-thread"}}),
            json!({"timestamp": timestamp(first_at), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-1"}}),
            json!({"timestamp": timestamp(first_at), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(10)}}}),
            json!({"timestamp": timestamp(first_at), "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-1"}}),
        ],
    );
    write_jsonl(
        &changed_path,
        &[
            json!({"timestamp": timestamp(second_at), "type": "session_meta", "payload": {"id": "shared-thread"}}),
            json!({"timestamp": timestamp(second_at), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-2"}}),
            json!({"timestamp": timestamp(second_at), "type": "turn_context", "payload": {"turn_id": "turn-2", "model": "gpt-cache"}}),
            json!({"timestamp": timestamp(second_at), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(15)}}}),
            json!({"timestamp": timestamp(second_at), "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-2"}}),
        ],
    );
    write_jsonl(
        &unrelated_path,
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "unrelated-thread"}
        })],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let before = cache.scan(&scan_config, now).unwrap();
    let shared_before = before
        .tasks
        .iter()
        .find(|task| task.thread_id == "shared-thread")
        .unwrap();
    assert_eq!(shared_before.token_usage.total_tokens, 15);

    append_jsonl(
        &changed_path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(20)}}
        })],
    );
    let cached = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cache.last_refresh().reused_files, 2);
    assert!(cache.last_refresh().rebuilt);
    assert_eq!(cache.last_refresh().tail_parsed_files, 1);
    assert_eq!(cache.last_refresh().full_parsed_files, 0);
    assert_eq!(cache.last_refresh().incrementally_reduced_threads, 1);
    assert!(!cache.last_refresh().full_rebuild);

    let fresh = scan_rollouts(&scan_config, now + chrono::Duration::seconds(2)).unwrap();
    assert_dataset_eq(&cached, &fresh);
    let shared = cached
        .tasks
        .iter()
        .find(|task| task.thread_id == "shared-thread")
        .unwrap();
    assert_eq!(shared.token_usage.total_tokens, 20);
    let last_turn = cached
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-2")
        .unwrap();
    assert_eq!(last_turn.token_usage.total_tokens, 10);
    assert_eq!(last_turn.model.as_deref(), Some("gpt-cache"));
}

#[test]
fn cache_preserves_foreign_parent_counter_baseline() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let child_id = "019f52c6-60d1-72e3-8f3f-b348d83da52e";
    let child_turn = "019f52c6-60d1-7000-8000-000000000001";
    let path = temp.path().join("sessions/rollout-child.jsonl");
    write_jsonl(
        &path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {
                "id": child_id,
                "timestamp": timestamp(now),
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": "019f52ac-7a9f-7fd1-8dda-e775ef950785",
                    "agent_role": null
                }}}
            }}),
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "019f52ac-7a9f-7fd1-8dda-e775ef950785"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(75)}}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(100)}}}),
        ],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let prefix = cache.scan(&scan_config, now).unwrap();
    assert_eq!(prefix.tasks[0].token_usage.total_tokens, 0);
    assert!(prefix.turns.is_empty());
    assert_eq!(cache.metrics().foreign_baseline_events, 1);

    append_jsonl(
        &path,
        &[
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "thread_settings_applied", "thread_settings": {"model": "gpt-child", "service_tier": null}}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": child_turn}}),
            json!({"timestamp": timestamp(now), "type": "turn_context", "payload": {"turn_id": child_turn, "model": "gpt-child"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(125)}}}),
        ],
    );

    let completed = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(cache.last_refresh().tail_parsed_files, 1);
    assert_eq!(completed.tasks[0].token_usage.total_tokens, 25);
    assert_eq!(completed.turns[0].token_usage.total_tokens, 25);
    assert_eq!(completed.turns[0].service_tier.as_deref(), Some("default"));
    assert_eq!(completed.calls[0].service_tier.as_deref(), Some("default"));

    let second = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();
    assert_eq!(cache.last_refresh().reused_files, 1);
    assert_eq!(second.tasks[0].token_usage.total_tokens, 25);
    assert_eq!(second.turns[0].service_tier.as_deref(), Some("default"));
    assert!(
        second
            .warnings
            .iter()
            .all(|warning| !warning.contains("counter reset"))
    );
}

#[test]
fn cached_refresh_clears_replay_diagnostics_when_a_changed_file_loses_its_owner() {
    let temp = TempDir::new().unwrap();
    let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
    let path = temp.path().join("sessions/rollout-ownerless-rewrite.jsonl");
    let retained_path = temp
        .path()
        .join("sessions/rollout-ownerless-rewrite-control.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {
                    "id": "owner-before-rewrite",
                    "timestamp": timestamp(now)
                }
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(100)}
                }
            }),
            json!({
                "timestamp": timestamp(now + chrono::Duration::milliseconds(1)),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(50)}
                }
            }),
        ],
    );
    write_jsonl(
        &retained_path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {
                    "id": "unchanged-owner",
                    "timestamp": timestamp(now)
                }
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(80)}
                }
            }),
            json!({
                "timestamp": timestamp(now + chrono::Duration::milliseconds(1)),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": usage(40)}
                }
            }),
        ],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let owned = cache
        .scan(&scan_config, now + chrono::Duration::milliseconds(2))
        .unwrap();
    assert_eq!(owned.stats.ambiguous_token_resets, 2);
    assert!(
        owned
            .warnings
            .iter()
            .any(|warning| warning.contains("token counter reset"))
    );

    write_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": "ignored-ownerless-turn",
                "padding": "this rewrite intentionally has no session metadata owner"
            }
        })],
    );

    let ownerless = cache
        .scan(&scan_config, now + chrono::Duration::seconds(2))
        .unwrap();
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cache.last_refresh().reused_files, 1);
    // The selected ownerless file makes the unchanged owner's first counter
    // boundary conservative, so that control file now contributes two
    // ambiguities. Neither diagnostic may come from the rewritten path.
    assert_eq!(
        ownerless.stats.ambiguous_token_resets, 2,
        "{:?}",
        ownerless.warnings
    );
    assert!(
        ownerless
            .warnings
            .iter()
            .filter(|warning| warning.contains("token counter reset"))
            .all(|warning| !warning.contains(&path.display().to_string()))
    );
    assert!(ownerless.warnings.iter().any(|warning| {
        warning.contains("token counter reset")
            && warning.contains(&retained_path.display().to_string())
    }));
    assert_eq!(ownerless.tasks.len(), 1);
    assert_eq!(ownerless.tasks[0].thread_id, "unchanged-owner");
    assert!(ownerless.turns.is_empty());

    let fresh = scan_rollouts(&scan_config, now + chrono::Duration::seconds(2)).unwrap();
    assert_dataset_eq(&ownerless, &fresh);
}

#[test]
fn cached_refresh_preserves_truncated_file_stats() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    for index in 0..3 {
        write_jsonl(
            &temp.path().join(format!("sessions/rollout-{index}.jsonl")),
            &[json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": format!("thread-{index}")}
            })],
        );
    }
    let mut scan_config = config(temp.path());
    scan_config.max_files = 2;
    let mut cache = RolloutCache::new();
    cache.scan(&scan_config, now).unwrap();
    let second = cache.scan(&scan_config, now).unwrap();

    assert_eq!(cache.last_refresh().reused_files, 2);
    assert_eq!(second.stats.discovered_files, 3);
    assert_eq!(second.stats.scanned_files, 2);
    assert_eq!(second.stats.truncated_files, 1);
}

#[test]
fn warm_discovery_reuses_inventory_and_detects_new_files() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    write_jsonl(
        &temp.path().join("sessions/rollout-first.jsonl"),
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "thread-first"}
        })],
    );
    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();

    let first = cache.scan(&scan_config, now).unwrap();
    assert_eq!(first.tasks.len(), 1);
    assert!(cache.last_refresh().discovery_full_scan);

    let unchanged = cache.scan(&scan_config, now).unwrap();
    assert_eq!(unchanged.tasks.len(), 1);
    assert!(cache.last_refresh().discovery_cache_hit);
    assert!(!cache.last_refresh().discovery_full_scan);
    assert_eq!(cache.last_refresh().discovery_probed_files, 1);

    // Directory timestamp resolution varies across filesystems. Cross a full
    // second before mutating the directory so this exercises cache
    // invalidation instead of the bounded periodic-rescan fallback.
    std::thread::sleep(Duration::from_millis(1_100));
    write_jsonl(
        &temp.path().join("sessions/rollout-second.jsonl"),
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "session_meta",
            "payload": {"id": "thread-second"}
        })],
    );
    let changed = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();

    assert_eq!(changed.tasks.len(), 2);
    assert!(cache.last_refresh().discovery_full_scan);
    assert!(cache.last_refresh().discovery_invalidated);
}

#[test]
fn cached_discovery_detects_a_rollout_root_appearing() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();

    let empty = cache.scan(&scan_config, now).unwrap();
    assert!(empty.tasks.is_empty());
    assert!(cache.last_refresh().discovery_full_scan);

    write_jsonl(
        &temp.path().join("sessions/rollout-new-root.jsonl"),
        &[json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(1)),
            "type": "session_meta",
            "payload": {"id": "thread-new-root"}
        })],
    );
    let changed = cache
        .scan(&scan_config, now + chrono::Duration::seconds(1))
        .unwrap();

    assert_eq!(changed.tasks.len(), 1);
    assert!(cache.last_refresh().discovery_full_scan);
    assert!(cache.last_refresh().discovery_invalidated);
}

#[test]
fn cold_scan_emits_aggregate_startup_stages_without_session_content() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-profiled.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "private-thread-id"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "private message text"}
            }),
        ],
    );
    let trace = StartupTrace::enabled(Instant::now(), None).unwrap();
    let mut scan_config = config(temp.path());
    scan_config.startup_trace = trace.clone();

    let mut cache = RolloutCache::new();
    cache.scan(&scan_config, now).unwrap();
    trace.stop();

    let report = trace.report();
    let stages = report
        .events
        .iter()
        .map(|event| event.stage.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "rollout.discover",
        "rollout.session_titles",
        "rollout.cache_maintenance",
        "rollout.cache_load",
        "rollout.parse_files",
        "rollout.cache_save",
        "rollout.reduce",
        "rollout.materialize",
        "rollout.total",
    ] {
        assert!(
            stages.contains(&expected),
            "missing startup stage {expected}"
        );
    }
    let parse = report
        .events
        .iter()
        .find(|event| event.stage == "rollout.parse_files")
        .unwrap();
    assert!(parse.detail.contains("reparsed=1"));
    assert!(parse.detail.contains("lines=2"));
    let cache_load = report
        .events
        .iter()
        .find(|event| event.stage == "rollout.cache_load")
        .unwrap();
    assert!(cache_load.detail.contains("enabled=false"));

    let serialized = trace.render_json().unwrap();
    assert!(!serialized.contains("private-thread-id"));
    assert!(!serialized.contains("private message text"));
    assert!(!serialized.contains(&temp.path().display().to_string()));
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_counted_and_retried() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-unreadable.jsonl");
    write_jsonl(
        &path,
        &[json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "unreadable-thread"}
        })],
    );
    let original_permissions = fs::metadata(&path).unwrap().permissions();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let first = cache.scan(&scan_config, now).unwrap();
    let second = cache.scan(&scan_config, now).unwrap();
    fs::set_permissions(&path, original_permissions).unwrap();

    assert_eq!(first.stats.scanned_files, 1);
    assert_eq!(first.stats.unreadable_files, 1);
    assert_eq!(second.stats.unreadable_files, 1);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cache.last_refresh().reused_files, 0);
}

#[test]
#[ignore = "reads the local Codex history and is intended for manual performance checks"]
fn benchmark_real_codex_cache() {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })
        .expect("CODEX_HOME or HOME is required");
    if !home.join("sessions").is_dir() {
        eprintln!("no local Codex sessions at {}", home.display());
        return;
    }
    let mut scan_config = config(&home);
    scan_config.lookback_days = 7;
    scan_config.max_files = 500;
    scan_config.redact_content = true;
    let mut cache = RolloutCache::new();

    let cold_started = Instant::now();
    let cold = cache.scan(&scan_config, Utc::now()).unwrap();
    let cold_elapsed = cold_started.elapsed();
    let warm_started = Instant::now();
    let warm = cache.scan(&scan_config, Utc::now()).unwrap();
    let warm_elapsed = warm_started.elapsed();
    let refresh = cache.last_refresh();
    eprintln!(
        "cold={cold_elapsed:?} warm={warm_elapsed:?} files={} lines={} calls={} turns={} discovery_hit={} probed_files={} probed_dirs={}",
        warm.stats.scanned_files,
        warm.stats.parsed_lines,
        warm.calls.len(),
        warm.turns.len(),
        refresh.discovery_cache_hit,
        refresh.discovery_probed_files,
        refresh.discovery_probed_dirs
    );
    assert_eq!(cold.stats.parsed_lines, warm.stats.parsed_lines);
    assert!(refresh.discovery_cache_hit);
    assert!(
        warm_elapsed < Duration::from_millis(200),
        "warm refresh took {warm_elapsed:?}"
    );
}
