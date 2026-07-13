use std::fs::{self, File};
use std::io::Write;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use codex_usage_monit::config::CollectConfig;
use codex_usage_monit::domain::{
    Confidence, Provenance, TaskRecord, TaskStatus, TokenUsage, TurnStatus,
};
use codex_usage_monit::rollout::scan_rollouts;
use serde_json::{Value, json};
use tempfile::TempDir;

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64, total: u64) -> Value {
    json!({
        "input_tokens": input,
        "cached_input_tokens": cached,
        "output_tokens": output,
        "reasoning_output_tokens": reasoning,
        "total_tokens": total
    })
}

fn write_jsonl(path: &std::path::Path, records: &[Value], malformed_tail: bool) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(path).unwrap();
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
    }
    if malformed_tail {
        writeln!(file, "{{ definitely not json").unwrap();
    }
}

fn config(home: &std::path::Path) -> CollectConfig {
    CollectConfig {
        codex_home: home.to_owned(),
        ..CollectConfig::default()
    }
}

fn simple_task_rollout(
    started_at: DateTime<Utc>,
    session_payload: Value,
    turn_id: &str,
    title: &str,
) -> Vec<Value> {
    vec![
        json!({
            "timestamp": timestamp(started_at),
            "type": "session_meta",
            "payload": session_payload
        }),
        json!({
            "timestamp": timestamp(started_at),
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "started_at": timestamp(started_at)
            }
        }),
        json!({
            "timestamp": timestamp(started_at + chrono::Duration::milliseconds(1)),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": title}
        }),
        json!({
            "timestamp": timestamp(started_at + chrono::Duration::milliseconds(2)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": turn_id}
        }),
    ]
}

#[test]
fn latest_session_index_title_overrides_the_first_message_without_bypassing_redaction() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-titled.jsonl");
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
                "payload": {"type": "user_message", "message": "Original first message"}
            }),
        ],
        false,
    );
    // Timestamp order, rather than file order, determines the latest rename.
    write_jsonl(
        &temp.path().join("session_index.jsonl"),
        &[
            json!({
                "id": "renamed-thread",
                "thread_name": "Current Desktop title",
                "updated_at": timestamp(now + chrono::Duration::seconds(2))
            }),
            json!({
                "id": "renamed-thread",
                "thread_name": "Older title written later",
                "updated_at": timestamp(now + chrono::Duration::seconds(1))
            }),
            json!({
                "id": "renamed-thread",
                "thread_name": "   ",
                "updated_at": timestamp(now + chrono::Duration::seconds(3))
            }),
        ],
        true,
    );

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    assert_eq!(dataset.tasks[0].title, "Current Desktop title");

    let mut redacted = config(temp.path());
    redacted.redact_content = true;
    let dataset = scan_rollouts(&redacted, now).unwrap();
    assert_eq!(dataset.tasks[0].title, "[redacted]");
}

#[test]
fn reconstructs_turn_deltas_ignores_duplicates_and_starts_a_new_epoch_on_reset() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let t0 = now - chrono::Duration::minutes(4);
    let t1 = now - chrono::Duration::minutes(3);
    let t2 = now - chrono::Duration::minutes(2);
    let path = temp
        .path()
        .join("sessions/2026/07/12/rollout-synthetic.jsonl");

    let records = vec![
        json!({
            "timestamp": timestamp(t0),
            "type": "session_meta",
            "payload": {
                "id": "thread-1",
                "timestamp": timestamp(t0),
                "cwd": "/work/project",
                "source": "vscode"
            }
        }),
        json!({
            "timestamp": timestamp(t0),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "turn-1", "started_at": t0.timestamp()}
        }),
        json!({
            "timestamp": timestamp(t0),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "  Investigate   the token counter  "}
        }),
        json!({
            "timestamp": timestamp(t0),
            "type": "turn_context",
            "payload": {"turn_id": "turn-1", "model": "gpt-test", "effort": "high"}
        }),
        json!({
            "timestamp": timestamp(t0 + chrono::Duration::seconds(5)),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": usage(8, 2, 2, 1, 10)},
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"used_percent": 12.0, "window_minutes": 300, "resets_at": (now + chrono::Duration::hours(2)).timestamp()},
                    "secondary": {"used_percent": 3.0, "window_minutes": 10080, "resets_at": (now + chrono::Duration::days(4)).timestamp()}
                }
            }
        }),
        // Codex can persist the same cumulative notification more than once.
        json!({
            "timestamp": timestamp(t0 + chrono::Duration::seconds(6)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(8, 2, 2, 1, 10)}}
        }),
        json!({
            "timestamp": timestamp(t0 + chrono::Duration::seconds(7)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(12, 3, 3, 1, 15)}}
        }),
        json!({
            "timestamp": timestamp(t1),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": "turn-1", "completed_at": t1.timestamp(), "duration_ms": 60_000}
        }),
        json!({
            "timestamp": timestamp(t1 + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "turn-2", "started_at": t1.timestamp() + 1}
        }),
        json!({
            "timestamp": timestamp(t1 + chrono::Duration::seconds(1)),
            "type": "turn_context",
            "payload": {"turn_id": "turn-2", "model": "gpt-next", "reasoning_effort": "medium"}
        }),
        json!({
            "timestamp": timestamp(t1 + chrono::Duration::seconds(3)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(18, 5, 4, 1, 22)}}
        }),
        // Every cumulative field moves backwards, indicating a new counter epoch.
        json!({
            "timestamp": timestamp(t1 + chrono::Duration::seconds(4)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(3, 0, 1, 0, 4)}}
        }),
        json!({
            "timestamp": timestamp(t1 + chrono::Duration::seconds(5)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(5, 1, 2, 0, 7)}}
        }),
        json!({
            "timestamp": timestamp(t2),
            "type": "event_msg",
            "payload": {"type": "turn_aborted", "turn_id": "turn-2", "reason": "interrupted", "completed_at": t2.timestamp()}
        }),
        // Unknown records are valid input and must not make the scan partial.
        json!({"timestamp": timestamp(t2), "type": "future_record", "payload": {"anything": true}}),
    ];
    write_jsonl(&path, &records, true);

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();

    assert_eq!(dataset.stats.discovered_files, 1);
    assert_eq!(dataset.stats.scanned_files, 1);
    assert_eq!(dataset.stats.parsed_lines, records.len());
    assert_eq!(dataset.stats.skipped_lines, 1);
    assert_eq!(dataset.stats.ambiguous_token_resets, 1);
    assert_eq!(dataset.tasks.len(), 1);
    assert_eq!(dataset.turns.len(), 2);
    assert_eq!(
        dataset.calls.len(),
        4,
        "duplicate cumulative usage was counted"
    );

    let task = &dataset.tasks[0];
    assert_eq!(task.title, "Investigate the token counter");
    assert_eq!(
        task.cwd.as_deref(),
        Some(std::path::Path::new("/work/project"))
    );
    assert_eq!(task.source.as_deref(), Some("vscode"));
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert_eq!(task.status_provenance, Provenance::LocalExact);
    assert_eq!(task.status_confidence, Confidence::High);
    assert_eq!(task.turn_count, 2);
    assert_eq!(
        task.token_usage,
        TokenUsage {
            input_tokens: 20,
            cached_input_tokens: 6,
            output_tokens: 5,
            reasoning_output_tokens: 1,
            total_tokens: 25,
        }
    );

    let turn_1 = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-1")
        .unwrap();
    assert_eq!(turn_1.status, TurnStatus::Completed);
    assert_eq!(turn_1.model.as_deref(), Some("gpt-test"));
    assert_eq!(turn_1.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        turn_1.message_preview.as_deref(),
        Some("Investigate the token counter")
    );
    assert_eq!(turn_1.token_usage.total_tokens, 15);
    assert_eq!(turn_1.duration_ms, Some(60_000));

    let turn_2 = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-2")
        .unwrap();
    assert_eq!(turn_2.status, TurnStatus::Interrupted);
    assert_eq!(turn_2.model.as_deref(), Some("gpt-next"));
    assert_eq!(turn_2.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(turn_2.message_preview, None);
    assert_eq!(turn_2.token_usage.total_tokens, 10);

    assert_eq!(dataset.rate_observations.len(), 1);
    let limits = &dataset.rate_observations[0];
    assert_eq!(limits.turn_id.as_deref(), Some("turn-1"));
    assert_eq!(
        limits.primary.as_ref().unwrap().window_duration_mins,
        Some(300)
    );
    assert_eq!(
        limits.secondary.as_ref().unwrap().window_duration_mins,
        Some(10_080)
    );
    assert!(
        dataset
            .warnings
            .iter()
            .any(|warning| warning.contains("counter reset"))
    );
    assert!(
        dataset
            .warnings
            .iter()
            .any(|warning| warning.contains("malformed JSON"))
    );
}

#[test]
fn captures_service_tier_at_turn_start_and_preserves_it_until_changed() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp
        .path()
        .join("sessions/2026/07/13/rollout-service-tier.jsonl");
    let event = |offset: i64, payload: Value| {
        json!({
            "timestamp": timestamp(now + chrono::Duration::seconds(offset)),
            "type": "event_msg",
            "payload": payload
        })
    };
    let records = vec![
        json!({
            "timestamp": timestamp(now),
            "type": "session_meta",
            "payload": {"id": "tier-thread", "timestamp": timestamp(now)}
        }),
        event(
            1,
            json!({"type": "task_started", "turn_id": "tierless-finished-turn"}),
        ),
        event(
            2,
            json!({"type": "task_complete", "turn_id": "tierless-finished-turn"}),
        ),
        event(
            3,
            json!({
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "default"}
            }),
        ),
        event(
            4,
            json!({"type": "task_started", "turn_id": "default-turn"}),
        ),
        event(
            5,
            json!({"type": "task_complete", "turn_id": "default-turn"}),
        ),
        event(
            6,
            json!({
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "priority"}
            }),
        ),
        event(7, json!({"type": "task_started", "turn_id": "fast-turn"})),
        event(8, json!({"type": "task_complete", "turn_id": "fast-turn"})),
        // Live rollouts do not necessarily repeat unchanged thread settings.
        event(
            9,
            json!({"type": "task_started", "turn_id": "inherited-fast-turn"}),
        ),
        event(
            10,
            json!({"type": "task_complete", "turn_id": "inherited-fast-turn"}),
        ),
        // Replayed activation events must not backfill a completed historical turn.
        event(
            11,
            json!({"type": "task_started", "turn_id": "tierless-finished-turn"}),
        ),
        event(
            12,
            json!({
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "future-tier"}
            }),
        ),
        event(
            13,
            json!({"type": "task_started", "turn_id": "unknown-tier-turn"}),
        ),
        event(
            14,
            json!({"type": "task_complete", "turn_id": "unknown-tier-turn"}),
        ),
    ];
    write_jsonl(&path, &records, false);

    let dataset = scan_rollouts(&config(temp.path()), now + chrono::Duration::seconds(15)).unwrap();
    let turn = |id: &str| {
        dataset
            .turns
            .iter()
            .find(|turn| turn.turn_id == id)
            .unwrap()
    };

    let default_turn = turn("default-turn");
    assert_eq!(default_turn.service_tier.as_deref(), Some("default"));
    assert!(!default_turn.is_fast());

    let tierless_finished_turn = turn("tierless-finished-turn");
    assert_eq!(tierless_finished_turn.service_tier, None);
    assert!(!tierless_finished_turn.is_fast());

    let fast_turn = turn("fast-turn");
    assert_eq!(fast_turn.service_tier.as_deref(), Some("priority"));
    assert!(fast_turn.is_fast());

    let inherited_fast_turn = turn("inherited-fast-turn");
    assert_eq!(
        inherited_fast_turn.service_tier.as_deref(),
        Some("priority")
    );
    assert!(inherited_fast_turn.is_fast());

    let unknown_tier_turn = turn("unknown-tier-turn");
    assert_eq!(
        unknown_tier_turn.service_tier.as_deref(),
        Some("future-tier")
    );
    assert!(!unknown_tier_turn.is_fast());
}

#[test]
fn resumes_the_parent_turn_after_a_nested_turn_completes() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-nested-turns.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "nested-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "outer"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": usage(8, 0, 2, 0, 10)}}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "inner"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "inner prompt"}
            }),
            // Steering messages do not replace the first preview for a turn.
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "inner steering"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "turn_context",
                "payload": {"turn_id": "outer", "model": "outer-model"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": usage(16, 0, 4, 0, 20)}}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "inner"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "outer followup"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": usage(24, 0, 6, 0, 30)}}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "outer"}
            }),
        ],
        false,
    );

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    let outer = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "outer")
        .unwrap();
    let inner = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "inner")
        .unwrap();

    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 30);
    assert_eq!(outer.token_usage.total_tokens, 20);
    assert_eq!(inner.token_usage.total_tokens, 10);
    assert_eq!(outer.status, TurnStatus::Completed);
    assert_eq!(inner.status, TurnStatus::Completed);
    assert_eq!(outer.message_preview.as_deref(), Some("outer followup"));
    assert_eq!(inner.message_preview.as_deref(), Some("inner prompt"));
}

#[test]
fn associates_only_active_or_explicit_messages_without_guessing_a_future_turn() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp
        .path()
        .join("sessions/rollout-message-association.jsonl");
    let long_message = "x".repeat(80);
    let unassigned_message = "t".repeat(120);
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "message-thread", "timestamp": timestamp(now)}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-1"}
            }),
            // With no active turn, this cannot be assigned safely.
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": unassigned_message}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-2"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-2"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-3"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": long_message}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-3"}
            }),
            // A future payload may provide an explicit turn id even after completion.
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "user_message", "turn_id": "turn-1", "message": "late explicit prompt"}
            }),
        ],
        false,
    );

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    let turn_1 = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-1")
        .unwrap();
    let turn_2 = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-2")
        .unwrap();
    let turn_3 = dataset
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-3")
        .unwrap();

    assert_eq!(
        turn_1.message_preview.as_deref(),
        Some("late explicit prompt")
    );
    assert_eq!(dataset.tasks[0].title.chars().count(), 96);
    assert!(dataset.tasks[0].title.ends_with("..."));
    assert_eq!(turn_2.message_preview, None);
    assert_eq!(turn_3.message_preview.as_ref().unwrap().chars().count(), 72);
    assert!(turn_3.message_preview.as_ref().unwrap().ends_with("..."));
}

#[test]
fn source_labels_distinguish_clients_roles_and_fallbacks() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let cases = vec![
        (
            "desktop-thread",
            json!({
                "id": "desktop-thread",
                "source": "vscode",
                "originator": "Codex Desktop",
                "thread_source": "user"
            }),
            "desktop",
        ),
        (
            "cli-thread",
            json!({
                "id": "cli-thread",
                "source": "cli",
                "originator": "codex-tui",
                "thread_source": "user"
            }),
            "cli",
        ),
        (
            "subagent-thread",
            json!({
                "id": "subagent-thread",
                "source": {"subagent": {"other": "worker"}},
                "originator": "Codex Desktop",
                "thread_source": "subagent"
            }),
            "subagent",
        ),
        (
            "fallback-thread",
            json!({"id": "fallback-thread", "source": "exec"}),
            "exec",
        ),
    ];

    for (index, (_, payload, _)) in cases.iter().enumerate() {
        write_jsonl(
            &temp
                .path()
                .join(format!("sessions/rollout-source-{index}.jsonl")),
            &[json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": payload
            })],
            false,
        );
    }

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    for (thread_id, _, expected) in cases {
        let task = dataset
            .tasks
            .iter()
            .find(|task| task.thread_id == thread_id)
            .unwrap();
        assert_eq!(task.source.as_deref(), Some(expected));
    }
}

#[test]
fn preserves_direct_parent_chains_and_metadata_fallback_priority() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let root_id = "root-thread";
    let child_id = "child-thread";
    let grandchild_id = "grandchild-thread";
    let nested_id = "nested-only-thread";
    let self_id = "self-parent-thread";
    let ordinary_fork_id = "ordinary-user-fork";

    let root_started = now - chrono::Duration::minutes(10);
    write_jsonl(
        &temp.path().join("sessions/rollout-root.jsonl"),
        &simple_task_rollout(
            root_started,
            json!({
                "id": root_id,
                "session_id": root_id,
                "timestamp": timestamp(root_started),
                "source": "vscode"
            }),
            "root-turn",
            "root task",
        ),
        false,
    );

    let child_started = now - chrono::Duration::minutes(8);
    write_jsonl(
        &temp.path().join("sessions/rollout-child.jsonl"),
        &simple_task_rollout(
            child_started,
            json!({
                "id": child_id,
                "session_id": root_id,
                "forked_from_id": root_id,
                "timestamp": timestamp(child_started),
                "source": {"subagent": {"other": "worker"}},
                "thread_source": "subagent"
            }),
            "child-turn",
            "child task",
        ),
        false,
    );

    let grandchild_started = now - chrono::Duration::minutes(6);
    write_jsonl(
        &temp.path().join("sessions/rollout-grandchild.jsonl"),
        &simple_task_rollout(
            grandchild_started,
            json!({
                "id": grandchild_id,
                "session_id": root_id,
                "parentThreadId": child_id,
                "forkedFromId": root_id,
                "timestamp": timestamp(grandchild_started),
                "source": {"subagent": {"other": "worker"}},
                "threadSource": "subagent"
            }),
            "grandchild-turn",
            "grandchild task",
        ),
        false,
    );

    let nested_started = now - chrono::Duration::minutes(4);
    write_jsonl(
        &temp.path().join("sessions/rollout-nested.jsonl"),
        &simple_task_rollout(
            nested_started,
            json!({
                "id": nested_id,
                "session_id": root_id,
                "parent_thread_id": root_id,
                "forked_from_id": root_id,
                "timestamp": timestamp(nested_started),
                "source": {"subAgent": {"threadSpawn": {
                    "parentThreadId": child_id
                }}},
                "threadSource": "subagent"
            }),
            "nested-turn",
            "nested fallback task",
        ),
        false,
    );

    let ordinary_started = now - chrono::Duration::minutes(3);
    write_jsonl(
        &temp.path().join("sessions/rollout-ordinary-fork.jsonl"),
        &simple_task_rollout(
            ordinary_started,
            json!({
                "id": ordinary_fork_id,
                "session_id": root_id,
                "forked_from_id": root_id,
                "timestamp": timestamp(ordinary_started),
                "source": "vscode",
                "thread_source": "user"
            }),
            "ordinary-fork-turn",
            "ordinary user fork",
        ),
        false,
    );

    let self_started = now - chrono::Duration::minutes(2);
    write_jsonl(
        &temp.path().join("sessions/rollout-self.jsonl"),
        &simple_task_rollout(
            self_started,
            json!({
                "id": self_id,
                "session_id": root_id,
                "parent_thread_id": self_id,
                "timestamp": timestamp(self_started),
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": self_id
                }}},
                "thread_source": "subagent"
            }),
            "self-turn",
            "self parent task",
        ),
        false,
    );

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    let parent_of = |thread_id: &str| {
        dataset
            .tasks
            .iter()
            .find(|task| task.thread_id == thread_id)
            .unwrap()
            .parent_thread_id
            .as_deref()
    };

    assert_eq!(parent_of(root_id), None);
    assert_eq!(parent_of(child_id), Some(root_id));
    assert_eq!(parent_of(grandchild_id), Some(child_id));
    assert_eq!(parent_of(nested_id), Some(child_id));
    assert_eq!(parent_of(ordinary_fork_id), None);
    assert_eq!(parent_of(self_id), None);
}

#[test]
fn upgrades_parent_priority_when_a_thread_spans_multiple_rollout_files() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let thread_id = "child-thread";
    let root_id = "root-thread";
    let direct_parent_id = "direct-parent-thread";
    let old_path = temp.path().join("sessions/rollout-01-old-fork.jsonl");
    let new_path = temp.path().join("sessions/rollout-02-new-nested.jsonl");

    write_jsonl(
        &old_path,
        &simple_task_rollout(
            now - chrono::Duration::minutes(2),
            json!({
                "id": thread_id,
                "forked_from_id": root_id,
                "timestamp": timestamp(now - chrono::Duration::minutes(2)),
                "source": {"subagent": {"other": "worker"}},
                "thread_source": "subagent"
            }),
            "old-turn",
            "old task",
        ),
        false,
    );
    write_jsonl(
        &new_path,
        &simple_task_rollout(
            now - chrono::Duration::minutes(1),
            json!({
                "id": thread_id,
                "parent_thread_id": root_id,
                "forked_from_id": root_id,
                "timestamp": timestamp(now - chrono::Duration::minutes(1)),
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": direct_parent_id
                }}},
                "thread_source": "subagent"
            }),
            "new-turn",
            "new task",
        ),
        false,
    );

    let modified = SystemTime::now();
    File::options()
        .write(true)
        .open(&old_path)
        .unwrap()
        .set_modified(modified - Duration::from_secs(2))
        .unwrap();
    File::options()
        .write(true)
        .open(&new_path)
        .unwrap()
        .set_modified(modified - Duration::from_secs(1))
        .unwrap();

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();
    let task = dataset
        .tasks
        .iter()
        .find(|task| task.thread_id == thread_id)
        .unwrap();

    assert_eq!(task.parent_thread_id.as_deref(), Some(direct_parent_id));
}

#[test]
fn task_record_parent_id_is_optional_in_legacy_json_and_uses_camel_case() {
    let legacy = json!({
        "threadId": "legacy-thread",
        "title": "legacy task",
        "cwd": null,
        "source": null,
        "createdAt": null,
        "updatedAt": null,
        "status": "completed",
        "statusProvenance": "local_exact",
        "statusConfidence": "high",
        "tokenUsage": {
            "inputTokens": 0,
            "cachedInputTokens": 0,
            "outputTokens": 0,
            "reasoningOutputTokens": 0,
            "totalTokens": 0
        },
        "turnCount": 0,
        "windowTokenUsage": {
            "inputTokens": 0,
            "cachedInputTokens": 0,
            "outputTokens": 0,
            "reasoningOutputTokens": 0,
            "totalTokens": 0
        },
        "localTokenSharePercent": 0.0,
        "estimatedQuotaPercent": 0.0,
        "quotaConfidence": "unknown"
    });

    let mut task: TaskRecord = serde_json::from_value(legacy).unwrap();
    assert_eq!(task.parent_thread_id, None);
    assert!(
        serde_json::to_value(&task)
            .unwrap()
            .get("parentThreadId")
            .is_none()
    );

    task.parent_thread_id = Some("parent-thread".to_string());
    assert_eq!(
        serde_json::to_value(task).unwrap()["parentThreadId"],
        "parent-thread"
    );
}

#[test]
fn ignores_embedded_parent_history_but_uses_its_cumulative_token_baseline() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let parent_started = now - chrono::Duration::minutes(20);
    let child_created = now - chrono::Duration::minutes(2);
    let path = temp.path().join("sessions/2026/07/12/rollout-child.jsonl");
    let child_id = "019f52c6-60d1-72e3-8f3f-b348d83da52e";
    let parent_id = "019f52ac-7a9f-7fd1-8dda-e775ef950785";
    let parent_turn = "019f52ad-ebf8-7820-88e0-57a3c1053e90";
    // Same UUIDv7 millisecond as the child thread but lexically smaller in the
    // random portion. Ownership must use the timestamp, not full UUID order.
    let child_turn = "019f52c6-60d1-7000-8000-000000000001";
    let records = vec![
        // A subagent rollout identifies its owner first.
        json!({
            "timestamp": timestamp(child_created),
            "type": "session_meta",
            "payload": {
                "id": child_id,
                "parent_thread_id": parent_id,
                "timestamp": timestamp(child_created),
                "cwd": "/work/child",
                "source": {"subagent": {"thread_spawn": {"parent_thread_id": parent_id}}}
            }
        }),
        // Some real rollouts replay parent events before the explicit parent
        // session_meta. Fork metadata must put the parser in foreign mode
        // immediately so these records only contribute a counter baseline.
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::microseconds(100)),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": parent_turn, "started_at": parent_started.timestamp()}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::microseconds(150)),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "foreign parent prompt"}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::microseconds(200)),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": usage(40, 15, 10, 2, 50)}}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::microseconds(300)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": parent_turn}
        }),
        // Codex then replays the parent's metadata and full history into the
        // same physical file before appending the child's own events.
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(1)),
            "type": "session_meta",
            "payload": {"id": parent_id, "timestamp": timestamp(parent_started), "source": "vscode"}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(2)),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": parent_turn, "started_at": parent_started.timestamp()}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(2)),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": usage(80, 30, 20, 4, 100)},
                "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 40, "window_minutes": 300}}
            }
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(3)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": parent_turn}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(4)),
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "priority"}
            }
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(300)),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": child_turn, "started_at": child_created.timestamp()}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(301)),
            "type": "turn_context",
            "payload": {"turn_id": child_turn, "model": "gpt-child", "effort": "high"}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::milliseconds(302)),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "implement the child task"}
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::seconds(1)),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": usage(100, 35, 25, 5, 125)},
                "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 41, "window_minutes": 300}}
            }
        }),
        json!({
            "timestamp": timestamp(child_created + chrono::Duration::seconds(2)),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": child_turn}
        }),
    ];
    write_jsonl(&path, &records, false);

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();

    assert_eq!(dataset.tasks.len(), 1);
    assert_eq!(dataset.tasks[0].thread_id, child_id);
    assert_eq!(
        dataset.tasks[0].parent_thread_id.as_deref(),
        Some(parent_id)
    );
    assert_eq!(dataset.tasks[0].title, "implement the child task");
    assert_eq!(dataset.tasks[0].source.as_deref(), Some("subagent"));
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 25);
    assert_eq!(dataset.turns.len(), 1);
    assert_eq!(dataset.turns[0].turn_id, child_turn);
    assert_eq!(
        dataset.turns[0].message_preview.as_deref(),
        Some("implement the child task")
    );
    assert_eq!(dataset.turns[0].token_usage.total_tokens, 25);
    assert_eq!(dataset.turns[0].service_tier, None);
    assert!(!dataset.turns[0].is_fast());
    assert_eq!(dataset.calls.len(), 1);
    assert_eq!(dataset.calls[0].tokens.total_tokens, 25);
    assert_eq!(dataset.rate_observations.len(), 1);
    assert_eq!(
        dataset.rate_observations[0]
            .primary
            .as_ref()
            .unwrap()
            .used_percent,
        41.0
    );
    assert!(
        dataset
            .warnings
            .iter()
            .all(|warning| !warning.contains("counter reset"))
    );
}

#[test]
fn attributes_a_final_token_update_after_task_completion_to_the_last_turn() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let path = temp.path().join("sessions/rollout-final-token.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": "thread-final"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-final"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "turn_context",
                "payload": {"turn_id": "turn-final", "model": "gpt-final"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": usage(8, 2, 2, 1, 10)}}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn-final"}
            }),
            json!({
                "timestamp": timestamp(now),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": usage(12, 3, 3, 1, 15)}}
            }),
        ],
        false,
    );

    let dataset = scan_rollouts(&config(temp.path()), now).unwrap();

    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 15);
    assert_eq!(dataset.turns[0].token_usage.total_tokens, 15);
    assert_eq!(dataset.turns[0].model.as_deref(), Some("gpt-final"));
    assert_eq!(dataset.calls.len(), 2);
    assert!(
        dataset
            .calls
            .iter()
            .all(|call| call.turn_id.as_deref() == Some("turn-final"))
    );
}

#[test]
fn scans_archived_sessions_filters_old_mtime_and_redacts_active_task_titles() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let active_at = now - chrono::Duration::seconds(10);
    let archived = temp.path().join("archived_sessions/rollout-archived.jsonl");
    write_jsonl(
        &archived,
        &[
            json!({
                "timestamp": timestamp(active_at),
                "type": "session_meta",
                "payload": {"id": "active-thread", "timestamp": timestamp(active_at), "source": {"subagent": {"name": "worker"}}}
            }),
            json!({
                "timestamp": timestamp(active_at),
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "this must not be retained"}
            }),
            json!({
                "timestamp": timestamp(active_at),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "active-turn", "started_at": active_at.timestamp()}
            }),
        ],
        false,
    );

    let old = temp.path().join("sessions/2025/01/01/rollout-old.jsonl");
    write_jsonl(
        &old,
        &[json!({
            "timestamp": "2025-01-01T00:00:00Z",
            "type": "session_meta",
            "payload": {"id": "old-thread"}
        })],
        false,
    );
    File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60))
        .unwrap();

    // Scanner roots are intentionally narrow; auth.json must never be opened.
    fs::write(
        temp.path().join("auth.json"),
        "not json and not readable as a rollout",
    )
    .unwrap();

    let mut scan_config = config(temp.path());
    scan_config.lookback_days = 1;
    scan_config.active_grace = Duration::from_secs(60);
    scan_config.redact_content = true;
    let dataset = scan_rollouts(&scan_config, now).unwrap();

    assert_eq!(dataset.stats.discovered_files, 1);
    assert_eq!(dataset.stats.scanned_files, 1);
    assert_eq!(dataset.stats.truncated_files, 0);
    assert_eq!(dataset.tasks.len(), 1);
    let task = &dataset.tasks[0];
    assert_eq!(task.thread_id, "active-thread");
    assert_eq!(task.title, "[redacted]");
    assert!(
        dataset
            .turns
            .iter()
            .all(|turn| turn.message_preview.is_none())
    );
    assert_eq!(task.source.as_deref(), Some("subagent"));
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.status_provenance, Provenance::Inferred);
    assert_eq!(task.status_confidence, Confidence::Medium);
    assert_eq!(dataset.turns[0].status, TurnStatus::InProgress);
}

#[test]
fn reports_files_truncated_by_the_scan_limit() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    for index in 0..3 {
        let path = temp.path().join(format!("sessions/rollout-{index}.jsonl"));
        write_jsonl(
            &path,
            &[json!({
                "timestamp": timestamp(now),
                "type": "session_meta",
                "payload": {"id": format!("thread-{index}")}
            })],
            false,
        );
    }

    let mut scan_config = config(temp.path());
    scan_config.max_files = 2;
    let dataset = scan_rollouts(&scan_config, now).unwrap();

    assert_eq!(dataset.stats.discovered_files, 3);
    assert_eq!(dataset.stats.scanned_files, 2);
    assert_eq!(dataset.stats.truncated_files, 1);
    assert_eq!(dataset.tasks.len(), 2);
}

#[test]
fn missing_codex_home_is_reported_as_incomplete() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("missing-codex-home");

    let dataset = scan_rollouts(&config(&missing), Utc::now()).unwrap();

    assert!(dataset.tasks.is_empty());
    assert_eq!(dataset.stats.unreadable_files, 1);
    assert!(
        dataset
            .warnings
            .iter()
            .any(|warning| warning.contains("no Codex rollout directories"))
    );
}

#[test]
fn an_extreme_lookback_does_not_overflow() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let mut scan_config = config(temp.path());
    scan_config.lookback_days = i64::MAX;

    let dataset = scan_rollouts(&scan_config, now).unwrap();

    assert_eq!(dataset.stats.discovered_files, 0);
    assert_eq!(dataset.stats.unreadable_files, 1);
}

#[test]
fn marks_an_unclosed_turn_stale_after_the_active_grace_period() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let old_activity = now - chrono::Duration::minutes(10);
    let path = temp.path().join("sessions/rollout-stale.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "timestamp": timestamp(old_activity),
                "type": "session_meta",
                "payload": {"id": "stale-thread", "timestamp": timestamp(old_activity)}
            }),
            json!({
                "timestamp": timestamp(old_activity),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "stale-turn"}
            }),
        ],
        false,
    );

    let mut scan_config = config(temp.path());
    scan_config.active_grace = Duration::from_secs(30);
    let dataset = scan_rollouts(&scan_config, now).unwrap();

    assert_eq!(dataset.tasks[0].status, TaskStatus::Stale);
    assert_eq!(dataset.tasks[0].status_provenance, Provenance::Stale);
    assert_eq!(dataset.tasks[0].status_confidence, Confidence::Low);
    assert_eq!(dataset.turns[0].status, TurnStatus::Stale);
}
