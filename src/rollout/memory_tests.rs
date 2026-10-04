use super::*;

fn fixture_config(home: &Path) -> CollectConfig {
    CollectConfig {
        codex_home: home.to_owned(),
        offline: true,
        active_grace: Duration::from_secs(3_600),
        ..CollectConfig::default()
    }
}

fn record(at: DateTime<Utc>, kind: &str, payload: Value) -> Value {
    serde_json::json!({"timestamp": at.to_rfc3339(), "type": kind, "payload": payload})
}

fn counter(at: DateTime<Utc>, total: u64) -> Value {
    record(
        at,
        "event_msg",
        serde_json::json!({
            "type": "token_count", "info": {"total_token_usage": {
                "input_tokens": total, "cached_input_tokens": 0,
                "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": total
            }}
        }),
    )
}

fn activity(at: DateTime<Utc>) -> Value {
    record(
        at,
        "response_item",
        serde_json::json!({"type": "function_call_output", "output": "ok"}),
    )
}

fn write_records(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}

fn append(path: &Path, value: Value) {
    writeln!(
        fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{value}"
    )
    .unwrap();
}

fn session(at: DateTime<Utc>, thread: &str, filler: usize) -> Vec<Value> {
    let mut records = vec![
        record(
            at,
            "session_meta",
            serde_json::json!({"id": thread, "timestamp": at.to_rfc3339()}),
        ),
        record(
            at,
            "event_msg",
            serde_json::json!({"type": "task_started", "turn_id": "turn"}),
        ),
        record(
            at,
            "turn_context",
            serde_json::json!({"turn_id": "turn", "model": "past-model"}),
        ),
        record(
            at,
            "event_msg",
            serde_json::json!({"type": "user_message", "turn_id": "turn", "message": "private fixture title"}),
        ),
    ];
    records.extend((0..filler).map(|_| activity(at)));
    records.push(counter(at + ChronoDuration::seconds(1), 100));
    records
}

fn assert_same_dataset(left: &RolloutDataset, right: &RolloutDataset) {
    assert_eq!(left.tasks, right.tasks);
    assert_eq!(left.turns, right.turns);
    assert_eq!(left.agent_interactions, right.agent_interactions);
    let sorted_calls = |dataset: &RolloutDataset| {
        let mut calls = dataset.calls.clone();
        calls.sort_by(|a, b| {
            (&a.thread_id, a.timestamp, &a.usage_event_id).cmp(&(
                &b.thread_id,
                b.timestamp,
                &b.usage_event_id,
            ))
        });
        calls
    };
    assert_eq!(sorted_calls(left), sorted_calls(right));
    assert_eq!(left.rate_observations, right.rate_observations);
    assert_eq!(left.stats, right.stats);
    assert_eq!(left.warnings, right.warnings);
}

fn event_pointer(cache: &RolloutCache, path: &Path, index: usize) -> *const ParsedEvent {
    cache.files[path].parsed.events.iter().nth(index).unwrap() as *const ParsedEvent
}

#[test]
fn rollout_memory_fork_widens_scan_without_widening_live_selection_or_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let recent = temp.path().join("sessions/rollout-recent.jsonl");
    let older = temp.path().join("sessions/rollout-older.jsonl");
    write_records(
        &recent,
        &session(now - ChronoDuration::minutes(1), "recent", 1),
    );
    let old_at = now - ChronoDuration::days(20);
    write_records(&older, &session(old_at, "older", 1));
    File::options()
        .write(true)
        .open(&older)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::from(old_at)))
        .unwrap();
    let mut live_config = fixture_config(temp.path());
    live_config.max_files = 1;
    let mut live = RolloutCache::new();
    let original = live
        .scan_with_external_boundary(&live_config, now, Some(now + ChronoDuration::minutes(1)))
        .unwrap();
    assert_eq!(original.tasks.len(), 1);
    assert_eq!(original.tasks[0].thread_id, "recent");
    assert_eq!(live.update_local_coverage(now, true), Some(now));
    let selected = live.selected.clone();
    let metrics = live.metrics;
    let refresh = live.last_refresh;
    let mut fork = live.fork_for_history_scan();
    assert!(Arc::ptr_eq(
        &live.files[&recent].parsed,
        &fork.files[&recent].parsed
    ));
    assert!(fork.selected.is_empty());
    assert!(fork.reduced.is_none());
    assert!(fork.as_of_reduced.is_none());
    assert!(fork.discovery_cache.is_none());
    assert!(fork.last_discovery.is_none());
    assert!(fork.dirty_files.is_empty());
    assert!(fork.disk_last_write.is_empty());
    assert!(fork.local_coverage_started_at.is_none());
    assert!(fork.last_materialized_at.is_none());
    assert!(fork.next_external_evidence_boundary.is_none());
    assert!(fork.startup_progress.is_none());
    let history_config = CollectConfig {
        lookback_days: 31,
        max_files: 500,
        ..live_config.clone()
    };
    let history = fork.scan(&history_config, now).unwrap();
    assert_eq!(history.tasks.len(), 2);
    assert_eq!(fork.last_refresh.reused_files, 1);
    assert_eq!(fork.last_refresh.full_parsed_files, 1);
    assert!(Arc::ptr_eq(
        &live.files[&recent].parsed,
        &fork.files[&recent].parsed
    ));
    assert_same_dataset(
        &history,
        &RolloutCache::new().scan(&history_config, now).unwrap(),
    );
    assert_eq!(live.files.len(), 1);
    assert_eq!(live.selected, selected);
    assert_eq!(live.metrics, metrics);
    assert_eq!(live.last_refresh, refresh);
    assert_eq!(live.discovery_cache.as_ref().unwrap().key.lookback_days, 7);
    assert_eq!(live.discovery_cache.as_ref().unwrap().key.max_files, 1);
    assert_eq!(live.local_coverage_started_at, Some(now));
    assert_eq!(
        live.next_external_evidence_boundary,
        Some(now + ChronoDuration::minutes(1))
    );
    assert_same_dataset(&original, &live.scan(&live_config, now).unwrap());
}

#[test]
fn rollout_memory_shared_append_keeps_full_chunks_and_unique_append_moves_buffer() {
    let temp = tempfile::tempdir().unwrap();
    let at = Utc::now() - ChronoDuration::minutes(1);
    let path = temp.path().join("sessions/rollout-tail.jsonl");
    // Four setup events + 509 Activities + one counter: two full chunks and
    // two events in the last chunk, which has room for subsequent appends.
    write_records(&path, &session(at, "tail", 509));
    let config = fixture_config(temp.path());
    let now = at + ChronoDuration::seconds(10);
    let mut live = RolloutCache::new();
    let initial = live.scan(&config, now).unwrap();
    assert_eq!(live.files[&path].parsed.events.len(), 514);
    let mut fork = live.fork_for_history_scan();
    assert_same_dataset(&initial, &fork.scan(&config, now).unwrap());
    let prefix = event_pointer(&live, &path, 0);
    let second_chunk = event_pointer(&live, &path, 256);
    let shared_last = event_pointer(&live, &path, 512);
    append(&path, counter(at + ChronoDuration::seconds(2), 200));
    let changed = live.scan(&config, now).unwrap();
    assert_eq!(live.last_refresh.tail_parsed_files, 1);
    assert_eq!(live.last_refresh.full_parsed_files, 0);
    assert_eq!(event_pointer(&live, &path, 0), prefix);
    assert_eq!(event_pointer(&live, &path, 256), second_chunk);
    assert_ne!(event_pointer(&live, &path, 512), shared_last);
    assert_eq!(event_pointer(&fork, &path, 512), shared_last);
    assert_eq!(fork.files[&path].parsed.events.len(), 514);
    assert_eq!(
        fork.reduced.as_ref().unwrap().dataset.calls[0]
            .tokens
            .total_tokens,
        100
    );
    assert_eq!(changed.tasks[0].token_usage.total_tokens, 200);
    assert_same_dataset(&changed, &RolloutCache::new().scan(&config, now).unwrap());
    assert_same_dataset(&changed, &fork.scan(&config, now).unwrap());
    assert_eq!(fork.last_refresh.tail_parsed_files, 1);
    assert_eq!(event_pointer(&fork, &path, 0), prefix);
    drop(fork);
    let unique_last = event_pointer(&live, &path, 512);
    append(
        &path,
        record(
            at + ChronoDuration::seconds(3),
            "event_msg",
            serde_json::json!({"type": "task_complete", "turn_id": "turn"}),
        ),
    );
    let completed = live.scan(&config, now).unwrap();
    assert_eq!(live.last_refresh.tail_parsed_files, 1);
    assert_eq!(
        event_pointer(&live, &path, 512),
        unique_last,
        "dropping a fork restores unique ownership: move the existing tail buffer"
    );
    assert_eq!(completed.tasks[0].status, TaskStatus::Completed);
    assert_same_dataset(&completed, &RolloutCache::new().scan(&config, now).unwrap());
}

#[test]
fn rollout_memory_replacement_and_failed_tail_guard_leave_fork_prefix_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let at = Utc::now() - ChronoDuration::minutes(1);
    let path = temp.path().join("sessions/rollout-replace.jsonl");
    write_records(&path, &session(at, "original", 270));
    let config = fixture_config(temp.path());
    let now = at + ChronoDuration::seconds(10);
    let mut live = RolloutCache::new();
    live.scan(&config, now).unwrap();
    let fork = live.fork_for_history_scan();
    let old_pointer = event_pointer(&fork, &path, 0);
    // Same inode, longer rewritten source: a tail candidate whose stored guard
    // fails must fall back to a full parse without touching the shared prefix.
    write_records(&path, &session(at, "rewritten", 320));
    let rewritten = live.scan(&config, now).unwrap();
    assert_eq!(live.last_refresh.full_parsed_files, 1);
    assert_eq!(live.last_refresh.tail_parsed_files, 0);
    assert_eq!(rewritten.tasks[0].thread_id, "rewritten");
    assert_same_dataset(&rewritten, &RolloutCache::new().scan(&config, now).unwrap());
    assert_eq!(
        fork.files[&path].parsed.owner_thread_id.as_deref(),
        Some("original")
    );
    assert_eq!(event_pointer(&fork, &path, 0), old_pointer);
    let replacement = temp.path().join("replacement.jsonl");
    write_records(&replacement, &session(at, "replacement", 400));
    // Windows cannot rename over an existing file; deletion and rename still
    // exercise stable file identity instead of treating a replacement as tail.
    fs::remove_file(&path).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let replaced = live.scan(&config, now).unwrap();
    assert_eq!(live.last_refresh.full_parsed_files, 1);
    assert_eq!(live.last_refresh.tail_parsed_files, 0);
    assert_eq!(replaced.tasks[0].thread_id, "replacement");
    assert_same_dataset(&replaced, &RolloutCache::new().scan(&config, now).unwrap());
    assert_eq!(event_pointer(&fork, &path, 0), old_pointer);
}

#[test]
fn rollout_memory_fork_redaction_home_and_as_of_projections_are_independent() {
    let temp = tempfile::tempdir().unwrap();
    let at = Utc::now() - ChronoDuration::minutes(1);
    let path = temp.path().join("sessions/rollout-as-of.jsonl");
    let future = at + ChronoDuration::seconds(30);
    let mut records = session(at, "as-of", 270);
    records.extend([
        record(future, "event_msg", serde_json::json!({"type": "thread_settings_applied", "thread_settings": {"service_tier": "fast"}})),
        record(future, "turn_context", serde_json::json!({"turn_id": "turn", "model": "future-model"})),
        counter(future + ChronoDuration::seconds(1), 200),
        record(at + ChronoDuration::seconds(2), "event_msg", serde_json::json!({"type": "task_complete", "turn_id": "turn", "completed_at": (future + ChronoDuration::seconds(2)).to_rfc3339()})),
    ]);
    write_records(&path, &records);
    let config = fixture_config(temp.path());
    let early = at + ChronoDuration::seconds(10);
    let late = future + ChronoDuration::seconds(3);
    let mut live = RolloutCache::new();
    let completed = live.scan(&config, late).unwrap();
    assert_eq!(completed.tasks[0].status, TaskStatus::Completed);
    let live_boundary = live.last_materialized_at;
    let mut fork = live.fork_for_history_scan();
    let pending = fork.scan(&config, early).unwrap();
    assert_eq!(fork.last_refresh.reused_files, 1);
    assert_eq!(pending.tasks[0].status, TaskStatus::Running);
    assert_eq!(pending.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(pending.turns[0].model.as_deref(), Some("past-model"));
    assert!(pending.turns[0].service_tier.is_none());
    assert!(pending.turns[0].completed_at.is_none());
    assert_same_dataset(&pending, &RolloutCache::new().scan(&config, early).unwrap());
    assert_eq!(live.last_materialized_at, live_boundary);
    assert_same_dataset(&completed, &fork.scan(&config, late).unwrap());
    let redacted_config = CollectConfig {
        redact_content: true,
        ..config.clone()
    };
    let redacted = fork.scan(&redacted_config, early).unwrap();
    assert_eq!(fork.last_refresh.full_parsed_files, 1);
    assert!(!Arc::ptr_eq(
        &live.files[&path].parsed,
        &fork.files[&path].parsed
    ));
    assert!(
        !serde_json::to_string(fork.files[&path].parsed.as_ref())
            .unwrap()
            .contains("private fixture title")
    );
    assert_same_dataset(
        &redacted,
        &RolloutCache::new().scan(&redacted_config, early).unwrap(),
    );
    let alternate = temp.path().join("other-home");
    let other_path = alternate.join("sessions/rollout-other.jsonl");
    write_records(&other_path, &session(at, "other", 1));
    let other_config = fixture_config(&alternate);
    let other = fork.scan(&other_config, early).unwrap();
    assert!(!fork.files.contains_key(&path));
    assert_eq!(other.tasks[0].thread_id, "other");
    assert_same_dataset(
        &other,
        &RolloutCache::new().scan(&other_config, early).unwrap(),
    );
    assert_same_dataset(&completed, &live.scan(&config, late).unwrap());
}

#[test]
fn rollout_memory_chunk_boundary_keeps_global_counter_indices_and_baseline_cow_local() {
    let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let mut events = ParsedEvents::default();
    events.push(ParsedEvent::TaskStarted {
        timestamp: at,
        turn_id: "turn".into(),
        started_at: None,
    });
    for _ in 1..255 {
        events.push(ParsedEvent::Activity { timestamp: at });
    }
    let usage = TokenUsage {
        input_tokens: 100,
        total_tokens: 100,
        ..TokenUsage::default()
    };
    events.push(ParsedEvent::RequestUsage(Box::new(RequestUsageEvent {
        timestamp: at,
        turn_id: "turn".into(),
        response_id: "request".into(),
        usage,
        turn_usage: None,
    })));
    events.push(ParsedEvent::TokenCount(Box::new(TokenCountEvent {
        timestamp: at,
        line_number: 257,
        total_usage: Some(usage),
        last_usage: Some(usage),
        rate_limits: None,
    })));
    assert_eq!(
        request_covered_counters(&events, None),
        HashSet::from([256])
    );
    assert_eq!(events.iter().enumerate().next_back().unwrap().0, 256);
    let mut events = ParsedEvents::default();
    for _ in 0..511 {
        events.push(ParsedEvent::Activity { timestamp: at });
    }
    events.push(ParsedEvent::ForeignCounterBaseline {
        timestamp: at,
        total_usage: usage,
    });
    events.push(ParsedEvent::SessionMeta {
        timestamp: at,
        payload: Map::new(),
    });
    events.push(ParsedEvent::ForeignThreadSettingsBaseline {
        timestamp: at,
        model: None,
        service_tier: None,
    });
    let shared = events.clone();
    let pointer =
        |events: &ParsedEvents, index| events.iter().nth(index).unwrap() as *const ParsedEvent;
    assert!(!retain_latest_foreign_baseline(
        &mut events,
        at + ChronoDuration::seconds(1),
        TokenUsage {
            input_tokens: 200,
            total_tokens: 200,
            ..TokenUsage::default()
        }
    ));
    assert_eq!(pointer(&events, 0), pointer(&shared, 0));
    assert_ne!(pointer(&events, 511), pointer(&shared, 511));
    assert_eq!(pointer(&events, 512), pointer(&shared, 512));
    let ParsedEvent::ForeignCounterBaseline { total_usage, .. } = shared.iter().nth(511).unwrap()
    else {
        panic!("baseline expected")
    };
    assert_eq!(total_usage.total_tokens, 100);
    assert_eq!(events.len(), shared.len());
}

#[test]
fn rollout_memory_shared_prefix_comparison_retains_float_value_semantics() {
    let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let mut events = ParsedEvents::default();
    events.push(ParsedEvent::TokenCount(Box::new(TokenCountEvent {
        timestamp: at,
        line_number: 1,
        total_usage: None,
        last_usage: None,
        rate_limits: Some(Box::new(CachedRateLimits {
            limit_id: "codex".into(),
            primary: Some(LimitWindow::new(f64::NAN, None, None)),
            secondary: None,
        })),
    })));
    let shared = events.clone();
    assert_ne!(events, shared, "PartialEq must also retain NaN != NaN");
    assert!(
        !events.starts_with(&shared),
        "shared identity must not hide NaN != NaN"
    );
    assert!(events.starts_with(&ParsedEvents::default()));
    assert!(!ParsedEvents::default().starts_with(&events));
}
