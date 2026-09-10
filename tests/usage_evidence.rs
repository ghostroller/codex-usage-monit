use std::fs::{self, File};
use std::io::Write;
use std::time::{Duration, SystemTime};

use chrono::{Duration as ChronoDuration, Utc};
use codex_usage_monit::config::CollectConfig;
use codex_usage_monit::rollout::RolloutCache;
use serde_json::{Value, json};
use tempfile::TempDir;

const THREAD: &str = "01a07687-1937-7e83-a60a-d898b2d2071a";
const TURN: &str = "01a0772b-fddb-7fa1-9710-df3ba60428be";

fn tokens(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": input + output})
}

fn native(id: &str, usage: Value, cumulative: Value) -> Value {
    json!({"type": "token_usage_record", "payload": {
        "thread_id": THREAD, "turn_id": TURN, "response_id": id,
        "usage": usage, "turn_token_usage": cumulative
    }})
}

fn counter(total: Value, last: Value) -> Value {
    json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
        "total_token_usage": total, "last_token_usage": last
    }}})
}

fn write_fixture(root: &std::path::Path, name: &str, events: &[Value]) -> std::path::PathBuf {
    fs::create_dir_all(root.join("sessions")).unwrap();
    let path = root.join("sessions").join(name);
    let mut file = File::create(&path).unwrap();
    let at = Utc::now() - ChronoDuration::minutes(1);
    for event in events {
        let mut event = event.clone();
        event["timestamp"] = json!(at);
        writeln!(file, "{event}").unwrap();
    }
    path
}

fn metadata() -> Vec<Value> {
    vec![
        json!({"type": "session_meta", "payload": {"id": THREAD, "source": {"subagent": {"other": "guardian"}}}}),
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": TURN}}),
        json!({"type": "turn_context", "payload": {"turn_id": TURN, "model": "gpt-5.6-luna"}}),
    ]
}

fn config(root: &std::path::Path) -> CollectConfig {
    CollectConfig {
        codex_home: root.to_owned(),
        lookback_days: 1,
        max_files: 500,
        offline: true,
        ..Default::default()
    }
}

#[test]
fn native_request_evidence_reconciles_guardian_and_legacy_records_without_double_counting() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    events.extend([
        native("response-one", tokens(251388, 918), tokens(251388, 918)),
        counter(tokens(5182700, 2975), json!({"total_tokens": 78865})),
        native("response-two", tokens(88107, 191), tokens(339495, 1109)),
        counter(tokens(5270807, 3166), tokens(88107, 191)),
    ]);
    write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let mut cache = RolloutCache::new();
    let first = cache.scan(&config(temp.path()), Utc::now()).unwrap();
    let second = cache.scan(&config(temp.path()), Utc::now()).unwrap();
    for dataset in [first, second] {
        assert_eq!(dataset.calls.len(), 2);
        assert_eq!(dataset.tasks[0].token_usage.total_tokens, 340604);
        assert_eq!(dataset.tasks[0].token_usage.input_tokens, 339495);
        assert!(dataset.calls.iter().all(|call| call.request_usage_exact));
        assert_eq!(dataset.stats.ambiguous_token_resets, 0);
    }
}

#[test]
fn native_request_evidence_deduplicates_copies_and_preserves_later_legacy_only_usage() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    let request = native("response-one", tokens(100, 10), tokens(100, 10));
    events.extend([
        request.clone(),
        request,
        counter(tokens(1000, 100), tokens(100, 10)),
        counter(tokens(1050, 105), tokens(50, 5)),
    ]);
    write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.calls.len(), 2);
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 165);
}

#[test]
fn guardian_without_request_evidence_does_not_claim_inherited_cumulative_tokens() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    events.push(counter(
        tokens(5182700, 2975),
        json!({"total_tokens": 78865}),
    ));
    write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 78865);
    assert_eq!(dataset.stats.ambiguous_token_resets, 1);
}

#[test]
fn copied_uuid_filename_does_not_make_unrelated_threads_exhaust_owner_budget() {
    let temp = TempDir::new().unwrap();
    for index in 0..70 {
        let owner = format!("00000000-0000-0000-0000-{index:012x}");
        let path = write_fixture(
            temp.path(),
            &format!("rollout-{owner}.jsonl"),
            &[json!({"type": "session_meta", "payload": {"id": owner}})],
        );
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 86400))
            .unwrap();
    }
    let events = [
        json!({"type": "session_meta", "payload": {"id": THREAD}}),
        counter(tokens(100, 10), tokens(10, 1)),
    ];
    write_fixture(
        temp.path(),
        &format!("rollout-{THREAD}_ffffffff-ffff-ffff-ffff-ffffffffffff.jsonl"),
        &events,
    );
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.stats.ambiguous_token_resets, 0);
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 110);
}

#[test]
fn native_request_evidence_keeps_uncovered_legacy_prefix() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    events.extend([
        counter(tokens(50, 5), tokens(50, 5)),
        native("later-request", tokens(20, 2), tokens(70, 7)),
        counter(tokens(70, 7), tokens(20, 2)),
    ]);
    write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.calls.len(), 2);
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 77);
}

#[test]
fn native_request_evidence_rejects_conflicting_identity_and_foreign_owner() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    let mut foreign = native("foreign", tokens(1000, 100), tokens(1000, 100));
    foreign["payload"]["thread_id"] = json!("parent");
    events.extend([
        native("conflict", tokens(10, 1), tokens(10, 1)),
        native("conflict", tokens(20, 2), tokens(20, 2)),
        native("good", tokens(30, 3), tokens(50, 5)),
        foreign,
    ]);
    write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.calls.len(), 1);
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 33);
    assert!(
        dataset
            .warnings
            .iter()
            .any(|warning| warning.contains("conflicting native request identity"))
    );
}

#[test]
fn native_request_evidence_does_not_suppress_earlier_counter_across_clock_boundary() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    events.push(counter(tokens(100, 10), tokens(100, 10)));
    let path = write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    let now = Utc::now();
    let mut future = native("future", tokens(100, 10), tokens(100, 10));
    future["timestamp"] = json!(now + ChronoDuration::hours(1));
    writeln!(File::options().append(true).open(path).unwrap(), "{future}").unwrap();
    let mut cache = RolloutCache::new();
    let later = cache
        .scan(&config(temp.path()), now + ChronoDuration::hours(2))
        .unwrap();
    assert_eq!(later.calls.len(), 1);
    assert!(
        later.calls[0]
            .usage_event_id
            .as_deref()
            .unwrap()
            .starts_with("usage-native-")
    );
    let earlier = cache.scan(&config(temp.path()), now).unwrap();
    assert_eq!(earlier.calls.len(), 1);
    assert!(
        !earlier.calls[0]
            .usage_event_id
            .as_deref()
            .unwrap()
            .starts_with("usage-native-")
    );
    assert_eq!(earlier.tasks[0].token_usage.total_tokens, 110);
}

#[test]
fn native_request_evidence_is_deduplicated_across_physical_rollout_copies() {
    let temp = TempDir::new().unwrap();
    let mut events = metadata();
    events.extend([
        native("response", tokens(100, 10), tokens(100, 10)),
        counter(tokens(1000, 100), tokens(100, 10)),
    ]);
    let original = write_fixture(temp.path(), &format!("rollout-{THREAD}.jsonl"), &events);
    fs::copy(
        original,
        temp.path().join("sessions").join(format!(
            "rollout-{THREAD}_ffffffff-ffff-ffff-ffff-ffffffffffff.jsonl"
        )),
    )
    .unwrap();
    let dataset = RolloutCache::new()
        .scan(&config(temp.path()), Utc::now())
        .unwrap();
    assert_eq!(dataset.calls.len(), 1);
    assert_eq!(dataset.tasks[0].token_usage.total_tokens, 110);
}

#[test]
fn partial_token_evidence_addition_preserves_unknown_independent_of_order() {
    use codex_usage_monit::domain::TokenUsage;
    let known = TokenUsage {
        input_tokens: 100,
        output_tokens: 10,
        total_tokens: 110,
        ..Default::default()
    };
    let unknown = TokenUsage {
        total_tokens: 50,
        ..Default::default()
    };
    let mut forward = known;
    forward.add_assign(unknown);
    let mut reverse = unknown;
    reverse.add_assign(known);
    assert_eq!(forward, reverse);
    assert!(forward.has_valid_breakdown());
    assert_eq!(forward.unclassified(), 50);
    assert_eq!(forward.delta_from(known).unwrap().unclassified(), 50);
    assert_eq!(
        serde_json::to_value(forward).unwrap()["unclassifiedTokens"],
        50
    );
}
