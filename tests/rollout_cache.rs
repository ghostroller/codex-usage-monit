use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use codex_usage_monit::config::CollectConfig;
use codex_usage_monit::domain::{RolloutDataset, TaskStatus};
use codex_usage_monit::rollout::{RolloutCache, scan_rollouts};
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

fn assert_dataset_eq(left: &RolloutDataset, right: &RolloutDataset) {
    assert_eq!(left.tasks, right.tasks);
    assert_eq!(left.turns, right.turns);
    assert_eq!(left.calls, right.calls);
    assert_eq!(left.rate_observations, right.rate_observations);
    assert_eq!(left.stats, right.stats);
    assert_eq!(left.warnings, right.warnings);
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
    let first_path = temp.path().join("sessions/rollout-a.jsonl");
    let changed_path = temp.path().join("sessions/rollout-b.jsonl");
    let unrelated_path = temp.path().join("sessions/rollout-c.jsonl");

    write_jsonl(
        &first_path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "shared-thread"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-1"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(10)}}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-1"}}),
        ],
    );
    write_jsonl(
        &changed_path,
        &[
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "shared-thread"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-2"}}),
            json!({"timestamp": timestamp(now), "type": "turn_context", "payload": {"turn_id": "turn-2", "model": "gpt-cache"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(15)}}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-2"}}),
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
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": child_id, "timestamp": timestamp(now)}}),
            json!({"timestamp": timestamp(now), "type": "session_meta", "payload": {"id": "019f52ac-7a9f-7fd1-8dda-e775ef950785"}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(100)}}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "task_started", "turn_id": child_turn}}),
            json!({"timestamp": timestamp(now), "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": usage(125)}}}),
        ],
    );

    let scan_config = config(temp.path());
    let mut cache = RolloutCache::new();
    let first = cache.scan(&scan_config, now).unwrap();
    assert_eq!(first.tasks[0].token_usage.total_tokens, 25);
    assert_eq!(first.turns[0].token_usage.total_tokens, 25);

    let second = cache.scan(&scan_config, now).unwrap();
    assert_eq!(cache.last_refresh().reused_files, 1);
    assert_eq!(second.tasks[0].token_usage.total_tokens, 25);
    assert!(
        second
            .warnings
            .iter()
            .all(|warning| !warning.contains("counter reset"))
    );
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
    eprintln!(
        "cold={cold_elapsed:?} warm={warm_elapsed:?} files={} lines={} calls={} turns={}",
        warm.stats.scanned_files,
        warm.stats.parsed_lines,
        warm.calls.len(),
        warm.turns.len()
    );
    assert_eq!(cold.stats.parsed_lines, warm.stats.parsed_lines);
    assert!(
        warm_elapsed < Duration::from_millis(200),
        "warm refresh took {warm_elapsed:?}"
    );
}
