use super::*;

// These fixtures use the pre-boxing externally tagged event representation.
// Keep them independent of the current Rust event constructors so a wrapper
// object or changed field spelling cannot silently update the expected wire.
const LEGACY_REQUEST_USAGE: &str = r#"{
    "RequestUsage": {
        "timestamp": "2026-10-04T12:00:00Z",
        "turn_id": "turn-old",
        "response_id": "response-old",
        "usage": {
            "inputTokens": 12,
            "cachedInputTokens": 4,
            "cacheWriteInputTokens": 2,
            "outputTokens": 3,
            "reasoningOutputTokens": 1,
            "unclassifiedTokens": 0,
            "totalTokens": 15
        },
        "turn_usage": {
            "inputTokens": 30,
            "cachedInputTokens": 10,
            "cacheWriteInputTokens": 6,
            "outputTokens": 9,
            "reasoningOutputTokens": 3,
            "unclassifiedTokens": 0,
            "totalTokens": 39
        }
    }
}"#;

const LEGACY_TOKEN_COUNT: &str = r#"{
    "TokenCount": {
        "timestamp": "2026-10-04T12:00:00Z",
        "line_number": 17,
        "total_usage": {
            "inputTokens": 30,
            "cachedInputTokens": 10,
            "cacheWriteInputTokens": 6,
            "outputTokens": 9,
            "reasoningOutputTokens": 3,
            "unclassifiedTokens": 0,
            "totalTokens": 39
        },
        "last_usage": {
            "inputTokens": 12,
            "cachedInputTokens": 4,
            "cacheWriteInputTokens": 2,
            "outputTokens": 3,
            "reasoningOutputTokens": 1,
            "unclassifiedTokens": 0,
            "totalTokens": 15
        },
        "rate_limits": {
            "limit_id": "codex",
            "primary": {
                "usedPercent": 25.5,
                "remainingPercent": 74.5,
                "windowDurationMins": 300,
                "resetsAt": "2026-10-04T17:00:00Z"
            },
            "secondary": null
        }
    }
}"#;

const LEGACY_TOKEN_COUNT_WITHOUT_LAST_USAGE: &str = r#"{
    "TokenCount": {
        "timestamp": "2026-10-04T12:00:00Z",
        "line_number": 8,
        "total_usage": {
            "inputTokens": 7,
            "cachedInputTokens": 0,
            "cacheWriteInputTokens": 0,
            "outputTokens": 2,
            "reasoningOutputTokens": 0,
            "unclassifiedTokens": 0,
            "totalTokens": 9
        },
        "rate_limits": null
    }
}"#;

fn legacy_parsed_file(events: Vec<Value>) -> Value {
    // The old required fields remain required; the omitted replay and tail
    // metadata exercise the existing serde defaults of older cache entries.
    let mut parsed: Value = serde_json::from_str(
        r#"{
            "owner_thread_id": "wire-thread",
            "activity_updated_at": "2026-10-04T12:00:00Z",
            "events": [],
            "parsed_lines": 4,
            "skipped_lines": 0,
            "unreadable_files": 0,
            "warnings": [],
            "complete": true
        }"#,
    )
    .unwrap();
    parsed["events"] = Value::Array(events);
    parsed
}

fn old_event_fixture(contents: &str) -> Value {
    serde_json::from_str(contents).unwrap()
}

// Preserve the old inline layouts independently of the boxed payload types.
// The variants are never instantiated: these are architecture-aware controls
// for the amount of storage previously reserved by each cached event slot.
#[allow(dead_code)]
struct LegacyInlineTokenCount {
    timestamp: DateTime<Utc>,
    line_number: usize,
    total_usage: Option<TokenUsage>,
    last_usage: Option<TokenUsage>,
    rate_limits: Option<CachedRateLimits>,
}

#[allow(dead_code)]
enum LegacyInlineParsedEvent {
    RequestUsage {
        timestamp: DateTime<Utc>,
        turn_id: String,
        response_id: String,
        usage: TokenUsage,
        turn_usage: Option<TokenUsage>,
    },
    SessionMeta {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    ForeignCounterBaseline {
        timestamp: DateTime<Utc>,
        total_usage: TokenUsage,
    },
    ForeignThreadSettingsBaseline {
        timestamp: DateTime<Utc>,
        model: Option<String>,
        service_tier: Option<String>,
    },
    UserMessage {
        timestamp: DateTime<Utc>,
        preview: String,
        turn_id: Option<String>,
        source: UserMessageSource,
    },
    AgentCall {
        requested_at: Option<DateTime<Utc>>,
        call_id: String,
        parent_turn_id: String,
        kind: AgentInteractionKind,
    },
    AgentActivity {
        recorded_at: Option<DateTime<Utc>>,
        occurred_at: Option<DateTime<Utc>>,
        call_id: String,
        child_thread_id: String,
        kind: AgentInteractionKind,
    },
    ThreadSettingsApplied {
        timestamp: DateTime<Utc>,
        service_tier: Option<String>,
    },
    Activity {
        timestamp: DateTime<Utc>,
    },
    TurnContext {
        timestamp: DateTime<Utc>,
        payload: Map<String, Value>,
    },
    TaskStarted {
        timestamp: DateTime<Utc>,
        turn_id: String,
        started_at: Option<DateTime<Utc>>,
    },
    TaskComplete {
        timestamp: DateTime<Utc>,
        turn_id: String,
        completed_at: Option<DateTime<Utc>>,
        duration_ms: Option<u64>,
    },
    TurnAborted {
        timestamp: DateTime<Utc>,
        turn_id: String,
        completed_at: Option<DateTime<Utc>>,
        duration_ms: Option<u64>,
        failed: bool,
    },
    TokenCount {
        timestamp: DateTime<Utc>,
        line_number: usize,
        total_usage: Option<TokenUsage>,
        last_usage: Option<TokenUsage>,
        rate_limits: Option<CachedRateLimits>,
    },
}

#[test]
fn rollout_memory_boxed_request_usage_preserves_legacy_json() {
    let event: ParsedEvent = serde_json::from_str(LEGACY_REQUEST_USAGE).unwrap();
    let ParsedEvent::RequestUsage(request) = &event else {
        panic!("legacy RequestUsage must still decode as a request event");
    };

    assert_eq!(request.turn_id, "turn-old");
    assert_eq!(request.response_id, "response-old");
    assert_eq!(request.usage.input_tokens, 12);
    assert_eq!(request.usage.cache_write_input_tokens, 2);
    assert_eq!(request.usage.total_tokens, 15);
    let turn_usage = request.turn_usage.unwrap();
    assert_eq!(turn_usage.cached_input_tokens, 10);
    assert_eq!(turn_usage.reasoning_output_tokens, 3);
    assert_eq!(turn_usage.total_tokens, 39);
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        old_event_fixture(LEGACY_REQUEST_USAGE)
    );
}

#[test]
fn rollout_memory_boxed_token_count_preserves_legacy_json() {
    let event: ParsedEvent = serde_json::from_str(LEGACY_TOKEN_COUNT).unwrap();
    let ParsedEvent::TokenCount(counter) = &event else {
        panic!("legacy TokenCount must still decode as a counter event");
    };

    assert_eq!(counter.line_number, 17);
    assert_eq!(counter.total_usage.unwrap().total_tokens, 39);
    assert_eq!(counter.last_usage.unwrap().cache_write_input_tokens, 2);
    let limits = counter.rate_limits.as_ref().unwrap();
    assert_eq!(limits.limit_id, "codex");
    let primary = limits.primary.as_ref().unwrap();
    assert_eq!(primary.used_percent, 25.5);
    assert_eq!(primary.remaining_percent, 74.5);
    assert_eq!(primary.window_duration_mins, Some(300));
    assert!(primary.resets_at.is_some());
    assert!(limits.secondary.is_none());
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        old_event_fixture(LEGACY_TOKEN_COUNT)
    );
}

#[test]
fn rollout_memory_legacy_token_count_keeps_last_usage_default() {
    let event: ParsedEvent = serde_json::from_str(LEGACY_TOKEN_COUNT_WITHOUT_LAST_USAGE).unwrap();
    let ParsedEvent::TokenCount(counter) = &event else {
        panic!("legacy counter without last_usage must still decode");
    };
    assert_eq!(counter.total_usage.unwrap().total_tokens, 9);
    assert!(counter.last_usage.is_none());

    let mut expected = old_event_fixture(LEGACY_TOKEN_COUNT_WITHOUT_LAST_USAGE);
    expected["TokenCount"]["last_usage"] = Value::Null;
    assert_eq!(serde_json::to_value(&event).unwrap(), expected);
}

#[test]
fn rollout_memory_parsed_and_cached_files_keep_flat_event_arrays() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    fs::write(&source, b"{}\n").unwrap();
    let discovered = inspect_rollout_file(&source).unwrap();

    for event_count in [0, 255, 256, 257] {
        let mut events = Vec::with_capacity(event_count);
        if event_count > 0 {
            events.push(old_event_fixture(LEGACY_REQUEST_USAGE));
        }
        for _ in 1..event_count.saturating_sub(1) {
            events.push(old_event_fixture(
                r#"{"Activity":{"timestamp":"2026-10-04T12:00:00Z"}}"#,
            ));
        }
        if event_count > 1 {
            events.push(old_event_fixture(LEGACY_TOKEN_COUNT));
        }
        assert_eq!(events.len(), event_count);
        let legacy = legacy_parsed_file(events.clone());
        let parsed: ParsedFile = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(parsed.events.len(), event_count);
        assert_eq!(parsed.source_lines, 0);
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(serialized["events"], Value::Array(events.clone()));

        // The Arc around ParsedFile must not add a wire wrapper either.
        let old_cached = serde_json::json!({
            "fingerprint": serde_json::to_value(&discovered.fingerprint).unwrap(),
            "parsed": legacy
        });
        let cached: CachedFile = serde_json::from_value(old_cached).unwrap();
        assert_eq!(cached.parsed.events.len(), event_count);
        let serialized_cached = serde_json::to_value(&cached).unwrap();
        assert_eq!(serialized_cached["parsed"]["events"], Value::Array(events));
        assert_eq!(
            serialized_cached["fingerprint"],
            serde_json::to_value(&discovered.fingerprint).unwrap()
        );
    }

    let mut missing_events = legacy_parsed_file(Vec::new());
    missing_events.as_object_mut().unwrap().remove("events");
    assert!(serde_json::from_value::<ParsedFile>(missing_events).is_err());
    let mut null_events = legacy_parsed_file(Vec::new());
    null_events["events"] = Value::Null;
    assert!(serde_json::from_value::<ParsedFile>(null_events).is_err());
}

#[test]
fn rollout_memory_pre_boxing_generation_is_an_actual_disk_cache_hit() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    let source_bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"wire-thread\"}}\n";
    fs::write(&source, source_bytes).unwrap();
    let discovered = inspect_rollout_file(&source).unwrap();
    let cache_root = temp.path().join("cache");
    let key = CacheKey {
        codex_home: temp.path().join("home"),
        redact_content: false,
    };
    let events = vec![
        old_event_fixture(LEGACY_REQUEST_USAGE),
        old_event_fixture(LEGACY_TOKEN_COUNT),
    ];
    // Only the platform-dependent fingerprint is generated. The generation,
    // envelope spellings, parsed object and event objects are old wire input.
    let old_entry = serde_json::json!({
        "formatVersion": 3,
        "parserRevision": 14,
        "key": {
            "codex_home": key.codex_home,
            "redact_content": false
        },
        "sourcePath": source,
        "sourceValidation": {
            "kind": "fullPrefixSha256",
            "sha256": Sha256::digest(source_bytes).to_vec()
        },
        "cached": {
            "fingerprint": serde_json::to_value(&discovered.fingerprint).unwrap(),
            "parsed": legacy_parsed_file(events.clone())
        }
    });
    let contents = serde_json::to_vec(&old_entry).unwrap();
    let entry_path = persistent_entry_path(&cache_root, &key, &source);
    write_private_atomically(&entry_path, &contents).unwrap();

    let mut hash_bytes = 0;
    let mut large_guard_bytes = 0;
    let mut tail_guard_bytes = 0;
    let loaded = load_persistent_file(
        &cache_root,
        &key,
        &discovered,
        &mut hash_bytes,
        &mut large_guard_bytes,
        &mut tail_guard_bytes,
    );
    let PersistentLoad::Exact {
        cached,
        bounded_guards,
        needs_migration,
    } = loaded
    else {
        panic!("format 3/parser 14 entry must remain a real exact disk hit");
    };
    assert!(!bounded_guards);
    assert!(!needs_migration);
    assert_eq!(
        cached.parsed.owner_thread_id.as_deref(),
        Some("wire-thread")
    );
    assert_eq!(cached.parsed.events.len(), 2);
    assert_eq!(
        serde_json::to_value(&cached).unwrap()["parsed"]["events"],
        Value::Array(events)
    );
    assert_eq!(hash_bytes, 0);
    assert_eq!(large_guard_bytes, 0);
    assert_eq!(tail_guard_bytes, 0);
    assert_eq!(fs::read(entry_path).unwrap(), contents);
}

#[test]
fn rollout_memory_inline_event_is_less_than_half_the_old_counter_payload() {
    // Before boxing, this whole counter payload (plus an enum discriminator)
    // occupied every Vec slot, including ordinary timestamp-only Activity.
    // Compare layouts on the current architecture rather than fixing one
    // platform's byte counts in a portable regression.
    let old_inline_lower_bound = std::mem::size_of::<LegacyInlineTokenCount>();
    let old_event_bytes = std::mem::size_of::<LegacyInlineParsedEvent>();
    let event_bytes = std::mem::size_of::<ParsedEvent>();
    let request_bytes = std::mem::size_of::<RequestUsageEvent>();
    let counter_bytes = std::mem::size_of::<TokenCountEvent>();
    let rate_bytes = std::mem::size_of::<CachedRateLimits>();
    println!(
        "rollout memory layout: ParsedEvent={event_bytes}, RequestUsageEvent={request_bytes}, TokenCountEvent={counter_bytes}, CachedRateLimits={rate_bytes}, legacyInlineTokenCount={old_inline_lower_bound}, legacyInlineParsedEvent={old_event_bytes}; dense request slot+payload={}, dense counter without quota slot+payload={}, dense counter with quota slot+payload={} bytes (excludes allocator metadata)",
        event_bytes + request_bytes,
        event_bytes + counter_bytes,
        event_bytes + counter_bytes + rate_bytes
    );
    assert!(
        event_bytes * 2 <= old_inline_lower_bound,
        "an Activity slot now uses {event_bytes} bytes; the old inline counter payload alone used {old_inline_lower_bound} bytes"
    );
    assert!(
        event_bytes * 2 <= old_event_bytes,
        "every inline event slot must stay at most half its pre-boxing size: current={event_bytes}, previous={old_event_bytes}"
    );
    assert!(
        event_bytes + counter_bytes <= old_event_bytes,
        "a counter without quota must not exceed its previous inline layout"
    );
}
