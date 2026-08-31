use super::*;
use chrono::TimeZone;

#[test]
fn fallback_usage_event_identity_is_path_independent_and_semantically_stable() {
    let timestamp = Utc
        .with_ymd_and_hms(2026, 8, 30, 10, 11, 12)
        .single()
        .unwrap();
    let cumulative = TokenUsage {
        input_tokens: 120,
        cached_input_tokens: 80,
        output_tokens: 9,
        reasoning_output_tokens: 4,
        total_tokens: 129,
        ..TokenUsage::default()
    };
    let last = TokenUsage {
        input_tokens: 20,
        cached_input_tokens: 10,
        output_tokens: 9,
        reasoning_output_tokens: 4,
        total_tokens: 29,
        ..TokenUsage::default()
    };

    let first = fallback_usage_event_id("thread-copy", timestamp, cumulative, Some(last));
    let copied = fallback_usage_event_id("thread-copy", timestamp, cumulative, Some(last));
    let changed_thread = fallback_usage_event_id("thread-other", timestamp, cumulative, Some(last));
    let changed_counter = fallback_usage_event_id(
        "thread-copy",
        timestamp,
        TokenUsage {
            total_tokens: cumulative.total_tokens + 1,
            ..cumulative
        },
        Some(last),
    );

    assert_eq!(first, copied);
    assert_ne!(first, changed_thread);
    assert_ne!(first, changed_counter);
    assert!(first.parse::<crate::source_history::UsageEventId>().is_ok());
    assert_eq!(first.len(), "usage-sha256-v1-".len() + 64);
}

#[test]
fn normalized_token_counter_event_is_exportable_as_exact_replica_identity() {
    let timestamp = Utc
        .with_ymd_and_hms(2026, 8, 30, 10, 11, 12)
        .single()
        .unwrap();
    let usage = TokenUsage {
        input_tokens: 120,
        cached_input_tokens: 80,
        output_tokens: 9,
        reasoning_output_tokens: 4,
        total_tokens: 129,
        ..TokenUsage::default()
    };
    let mut thread = ThreadBuilder {
        thread_id: "thread-copy".to_owned(),
        ..ThreadBuilder::default()
    };
    let mut dataset = RolloutDataset::default();

    apply_token_count(
        &mut thread,
        TokenCounterSample {
            total: Some(usage),
            last: Some(usage),
        },
        None,
        timestamp,
        Path::new("/copy/rollout.jsonl"),
        7,
        &mut dataset,
    );

    assert_eq!(dataset.calls.len(), 1);
    assert!(dataset.calls[0].usage_event_identity_exact);
    assert!(dataset.calls[0].request_usage_exact);
    assert_eq!(
        dataset.calls[0].usage_event_id.as_deref(),
        Some(fallback_usage_event_id(
            "thread-copy",
            timestamp,
            usage,
            Some(usage)
        ))
        .as_deref()
    );
}

fn prepare_private_cache_fixture_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn write_private_cache_fixture(path: &Path, contents: impl AsRef<[u8]>) {
    prepare_private_cache_fixture_directory(path.parent().unwrap());
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn windows_rollout_identity_requires_matching_volume_and_full_file_id() {
    let first = [1_u8; 16];
    let mut replacement = first;
    replacement[15] = 2;

    assert!(windows_stable_identity_matches(
        Some(7),
        Some(first),
        Some(7),
        Some(first)
    ));
    assert!(!windows_stable_identity_matches(
        Some(7),
        Some(first),
        Some(8),
        Some(first)
    ));
    assert!(!windows_stable_identity_matches(
        Some(7),
        Some(first),
        Some(7),
        Some(replacement)
    ));
    assert!(!windows_stable_identity_matches(None, None, None, None));

    assert!(windows_snapshot_identity_matches(
        11, 12, None, None, 11, 12, None, None,
    ));
    assert!(!windows_snapshot_identity_matches(
        11,
        12,
        Some(7),
        Some(first),
        11,
        12,
        None,
        None,
    ));
    assert!(!windows_snapshot_identity_matches(
        11, 12, None, None, 13, 12, None, None,
    ));
    assert!(!windows_snapshot_identity_matches(
        11,
        12,
        Some(7),
        Some(first),
        11,
        12,
        Some(7),
        Some(replacement),
    ));
}

#[test]
fn oversized_rollout_lines_are_drained_before_parsing_the_next_record() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rollout.jsonl");
    fs::write(&path, b"fixture").unwrap();
    let file = inspect_rollout_file(&path).unwrap();
    let input = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread\"}}}}\n{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn\"}}}}\n",
        "x".repeat(512)
    );
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };

    let parsed = parse_rollout_reader_with_limit(
        &file,
        &config,
        ParsedFile {
            replay_timestamps_complete: true,
            ..ParsedFile::default()
        },
        BufReader::with_capacity(17, input.as_bytes()),
        256,
    );

    assert!(parsed.complete);
    assert_eq!(parsed.source_lines, 3);
    assert_eq!(parsed.parsed_lines, 2);
    assert_eq!(parsed.skipped_lines, 1);
    assert_eq!(parsed.owner_thread_id.as_deref(), Some("thread"));
    assert!(parsed.warnings[0].contains("oversized JSON"));
    assert!(parsed.events.iter().any(|event| matches!(
        event,
        ParsedEvent::TaskStarted { turn_id, .. } if turn_id == "turn"
    )));
}

#[test]
fn malformed_rollout_warning_floods_are_bounded_and_summarized() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("rollout.jsonl");
    let mut input = "not json\n".repeat(ROLLOUT_MAX_WARNINGS_PER_FILE + 16);
    input.push_str("{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\"}}\n");
    fs::write(path, input).unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };

    let mut cache = RolloutCache::new();
    let dataset = cache.scan(&config, Utc::now()).unwrap();

    assert!(dataset.warnings.len() <= ROLLOUT_MAX_WARNINGS);
    assert_eq!(
        dataset.warnings.last().map(String::as_str),
        Some("suppressed 16 additional rollout warnings")
    );
    assert_eq!(
        dataset.stats.skipped_lines,
        ROLLOUT_MAX_WARNINGS_PER_FILE + 16
    );
    assert_eq!(dataset.tasks.len(), 1);
}

#[test]
fn warning_collection_is_bounded_before_materialization() {
    let mut warnings = Vec::new();
    let mut suppressed = 0_usize;

    for index in 0..1_000 {
        push_bounded_rollout_warning(&mut warnings, &mut suppressed, format!("warning {index}"));
    }

    assert_eq!(warnings.len(), ROLLOUT_MAX_WARNINGS - 1);
    assert_eq!(suppressed, 1_000 - (ROLLOUT_MAX_WARNINGS - 1));
}

#[test]
fn continuous_local_coverage_materializes_at_quarter_hour_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("sessions")).unwrap();
    let boundary = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let initial_at = boundary - ChronoDuration::seconds(10);
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    assert!(
        cache
            .scan_if_changed(&config, initial_at)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        cache.update_local_coverage(initial_at, true),
        Some(initial_at)
    );
    assert!(
        cache
            .scan_if_changed(&config, boundary - ChronoDuration::seconds(1))
            .unwrap()
            .is_none()
    );
    assert!(cache.scan_if_changed(&config, boundary).unwrap().is_some());
    assert_eq!(
        cache.update_local_coverage(boundary, true),
        Some(initial_at)
    );

    let rolled_back = initial_at - ChronoDuration::seconds(1);
    assert_eq!(
        cache.update_local_coverage(rolled_back, true),
        Some(rolled_back)
    );
    cache.discovery_cache.as_mut().unwrap().complete = false;
    assert_eq!(cache.update_local_coverage(boundary, true), None);
    assert!(cache.local_coverage_last_complete_at.is_none());
}

#[test]
fn external_as_of_boundary_materializes_without_rollout_changes() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("sessions")).unwrap();
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let boundary = now + ChronoDuration::seconds(10);
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    assert!(
        cache
            .scan_if_changed_with_external_boundary(&config, now, Some(boundary))
            .unwrap()
            .is_some()
    );
    assert!(
        cache
            .scan_if_changed_with_external_boundary(
                &config,
                boundary - ChronoDuration::seconds(1),
                Some(boundary),
            )
            .unwrap()
            .is_none()
    );
    assert!(
        cache
            .scan_if_changed_with_external_boundary(&config, boundary, None)
            .unwrap()
            .is_some()
    );
}

#[test]
fn local_coverage_restarts_after_a_gap_longer_than_the_scan_lookback() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("sessions")).unwrap();
    let initial_at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        lookback_days: 1,
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();
    cache.scan(&config, initial_at).unwrap();

    assert_eq!(
        cache.update_local_coverage(initial_at, true),
        Some(initial_at)
    );
    let within_lookback = initial_at + ChronoDuration::hours(23);
    assert_eq!(
        cache.update_local_coverage(within_lookback, true),
        Some(initial_at)
    );

    let after_gap = within_lookback + ChronoDuration::days(1) + ChronoDuration::seconds(1);
    assert_eq!(
        cache.update_local_coverage(after_gap, true),
        Some(after_gap)
    );
    assert_eq!(cache.local_coverage_last_complete_at, Some(after_gap));
}

#[test]
fn incomplete_discovery_inventory_retries_with_a_bounded_backoff() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let now = Utc::now();
    fs::write(
        sessions.join("rollout.jsonl"),
        format!(
            "{{\"timestamp\":\"{}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread\"}}}}\n",
            now.to_rfc3339()
        ),
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();
    cache.scan(&config, now).unwrap();
    assert!(cache.last_refresh().discovery_complete);
    let discovery = cache.discovery_cache.as_mut().unwrap();
    discovery.complete = false;
    discovery.unreadable_files = 1;
    discovery.warnings = vec!["temporary discovery failure".to_string()];
    discovery.full_scan_at = Instant::now();
    let mut source = std::fs::OpenOptions::new()
        .append(true)
        .open(sessions.join("rollout.jsonl"))
        .unwrap();
    writeln!(
        source,
        "{{\"timestamp\":\"{}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn\"}}}}",
        (now + ChronoDuration::seconds(1)).to_rfc3339()
    )
    .unwrap();

    let cached = cache
        .scan(&config, now + ChronoDuration::seconds(1))
        .unwrap();
    assert!(cache.last_refresh().discovery_cache_hit);
    assert!(!cache.last_refresh().discovery_full_scan);
    assert!(!cache.last_refresh().discovery_complete);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert_eq!(cached.turns.len(), 1);
    assert_eq!(cached.stats.unreadable_files, 1);
    assert_eq!(
        cached.warnings,
        vec!["temporary discovery failure".to_string()]
    );

    cache.discovery_cache.as_mut().unwrap().full_scan_at = Instant::now()
        .checked_sub(DISCOVERY_INCOMPLETE_RESCAN_INTERVAL)
        .unwrap();
    let retried = cache.scan(&config, now).unwrap();
    assert!(cache.last_refresh().discovery_full_scan);
    assert!(cache.last_refresh().discovery_complete);
    assert_eq!(retried.stats.unreadable_files, 0);
    assert!(retried.warnings.is_empty());
}

#[test]
fn incomplete_discovery_makes_the_selected_counter_boundary_partial() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let now = Utc::now();
    fs::write(
        sessions.join("rollout.jsonl"),
        format!(
            "{{\"timestamp\":\"{}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread\"}}}}\n{{\"timestamp\":\"{}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":10,\"cached_input_tokens\":0,\"output_tokens\":0,\"reasoning_output_tokens\":0,\"total_tokens\":10}}}}}}}}\n",
            now.to_rfc3339(),
            (now + ChronoDuration::seconds(1)).to_rfc3339(),
        ),
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let complete = cache
        .scan(&config, now + ChronoDuration::seconds(1))
        .unwrap();
    assert_eq!(complete.calls.len(), 1);
    let discovery = cache.discovery_cache.as_mut().unwrap();
    discovery.complete = false;
    discovery.unreadable_files = 1;
    discovery.full_scan_at = Instant::now();

    let incomplete = cache
        .scan(&config, now + ChronoDuration::seconds(2))
        .unwrap();
    assert!(cache.last_refresh().discovery_cache_hit);
    assert!(incomplete.calls.is_empty());
    assert_eq!(incomplete.tasks[0].token_usage.total_tokens, 0);
    assert_eq!(incomplete.stats.ambiguous_token_resets, 1);
}

#[test]
fn pruning_removes_entries_until_within_bounds() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("a.json");
    let second = temp.path().join("b.json");
    let third = temp.path().join("c.json");
    write_private_cache_fixture(&first, b"one");
    write_private_cache_fixture(&second, b"two");
    write_private_cache_fixture(&third, b"three");

    let pruned = prune_cache_directory(temp.path(), None, 1, u64::MAX, SystemTime::UNIX_EPOCH);

    assert_eq!(pruned.entries, 2);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn pruning_removes_only_stale_cache_temporary_files() {
    let temp = tempfile::tempdir().unwrap();
    let stale = temp.path().join(".0123456789abcdef.json.42.7.tmp");
    let unrelated = temp.path().join("notes.tmp");
    write_private_cache_fixture(&stale, b"partial");
    write_private_cache_fixture(&unrelated, b"keep");

    let pruned = prune_cache_directory(
        temp.path(),
        None,
        usize::MAX,
        u64::MAX,
        SystemTime::now() + Duration::from_secs(1),
    );

    assert_eq!(pruned.stale_temps, 1);
    assert!(!stale.exists());
    assert!(unrelated.exists());
}

#[test]
fn write_budget_reserves_atomic_temporary_file_space_before_each_entry() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("a.json");
    let target = temp.path().join("b.json");
    write_private_cache_fixture(&first, [0_u8; 8]);
    write_private_cache_fixture(&target, [0_u8; 8]);
    let mut budget = PersistentCacheBudget {
        directory: temp.path().to_owned(),
        usage: PersistentCacheUsage {
            entries: 2,
            bytes: 16,
        },
        max_entries: 2,
        max_bytes: 16,
        pruned_entries: 0,
        pruned_temps: 0,
    };

    let old_bytes = budget.reserve(&target, 8).unwrap();

    assert_eq!(old_bytes, Some(8));
    assert_eq!(budget.usage.bytes, 8);
    assert_eq!(budget.pruned_entries, 1);
    assert!(!first.exists());
    assert!(target.exists());
    budget.commit(old_bytes, 8);
    assert_eq!(budget.usage.entries, 1);
    assert_eq!(budget.usage.bytes, 8);
}

#[test]
fn oversized_entry_is_suppressed_after_the_first_serialization() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    fs::write(&source, b"{}\n").unwrap();
    let fingerprint =
        FileFingerprint::from_path_and_metadata(&source, &source.metadata().unwrap()).unwrap();
    let key = CacheKey {
        codex_home: temp.path().join("home"),
        redact_content: false,
    };
    let config = CollectConfig {
        rollout_cache_dir: Some(temp.path().join("cache")),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();
    cache.files.insert(
        source.clone(),
        CachedFile {
            fingerprint,
            parsed: ParsedFile {
                complete: true,
                ..ParsedFile::default()
            },
        },
    );
    cache.dirty_files.insert(source.clone());

    let first = cache.persist_dirty_files_with_limit(&config, &key, 0);
    assert_eq!(first.oversized, 1);
    assert_eq!(first.failures, 0);
    assert!(cache.unpersistable_files.contains_key(&source));

    cache.dirty_files.insert(source.clone());
    let second = cache.persist_dirty_files_with_limit(&config, &key, 0);
    assert_eq!(second.oversized, 1);
    assert_eq!(second.failures, 0);

    fs::write(&source, b"").unwrap();
    let fingerprint =
        FileFingerprint::from_path_and_metadata(&source, &source.metadata().unwrap()).unwrap();
    cache.files.insert(
        source.clone(),
        CachedFile {
            fingerprint,
            parsed: ParsedFile {
                complete: true,
                ..ParsedFile::default()
            },
        },
    );
    cache.dirty_files.insert(source);
    let after_shrink = cache.persist_dirty_files_with_limit(&config, &key, 4096);
    assert_eq!(after_shrink.written, 1);
    assert!(cache.unpersistable_files.is_empty());
}

#[test]
fn persistent_hit_is_rejected_if_source_changed_after_discovery() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    fs::write(&source, b"{}\n").unwrap();
    let discovered = inspect_rollout_file(&source).unwrap();
    let key = CacheKey {
        codex_home: temp.path().join("home"),
        redact_content: false,
    };
    let cached = CachedFile {
        fingerprint: discovered.fingerprint.clone(),
        parsed: ParsedFile {
            complete: true,
            ..ParsedFile::default()
        },
    };
    let cache_root = temp.path().join("cache");
    assert!(
        persist_file_entry(
            &cache_root,
            &key,
            &source,
            &cached,
            MAX_PERSISTENT_ENTRY_BYTES,
        )
        .is_ok()
    );

    fs::write(&source, b"{}\n{}\n").unwrap();

    let mut hash_bytes = 0;
    let mut large_guard_bytes = 0;
    let mut tail_guard_bytes = 0;
    assert!(matches!(
        load_persistent_file(
            &cache_root,
            &key,
            &discovered,
            &mut hash_bytes,
            &mut large_guard_bytes,
            &mut tail_guard_bytes,
        ),
        PersistentLoad::Miss
    ));
    assert_eq!(hash_bytes, 0);
    assert_eq!(large_guard_bytes, 0);
    assert_eq!(tail_guard_bytes, 0);
}

#[test]
fn persistent_exact_hit_uses_the_metadata_only_fast_path() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    fs::write(&source, b"old-content\n").unwrap();
    let old = inspect_rollout_file(&source).unwrap();
    let key = CacheKey {
        codex_home: temp.path().join("home"),
        redact_content: false,
    };
    let cached = CachedFile {
        fingerprint: old.fingerprint,
        parsed: ParsedFile {
            complete: true,
            ..ParsedFile::default()
        },
    };
    let cache_root = temp.path().join("cache");
    persist_file_entry(
        &cache_root,
        &key,
        &source,
        &cached,
        MAX_PERSISTENT_ENTRY_BYTES,
    )
    .unwrap();

    let current = inspect_rollout_file(&source).unwrap();
    let mut hash_bytes = 0;
    let mut large_guard_bytes = 0;
    let mut tail_guard_bytes = 0;
    assert!(matches!(
        load_persistent_file(
            &cache_root,
            &key,
            &current,
            &mut hash_bytes,
            &mut large_guard_bytes,
            &mut tail_guard_bytes,
        ),
        PersistentLoad::Exact { .. }
    ));
    assert_eq!(hash_bytes, 0);
    assert_eq!(large_guard_bytes, 0);
    assert_eq!(tail_guard_bytes, 0);
}

#[test]
fn persistent_prefix_hash_refuses_io_above_the_limit() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    fs::write(&source, b"small").unwrap();
    let mut hash_bytes = 0;

    let error = sha256_file_prefix(
        &source,
        MAX_PERSISTENT_PREFIX_HASH_BYTES + 1,
        &mut hash_bytes,
    )
    .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(hash_bytes, 0);
}

#[test]
fn persistent_entry_above_the_full_hash_limit_uses_bounded_guards() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    let file = File::create(&source).unwrap();
    file.set_len(MAX_PERSISTENT_PREFIX_HASH_BYTES + 1).unwrap();
    let fingerprint = inspect_rollout_file(&source).unwrap().fingerprint;
    let cached = CachedFile {
        fingerprint,
        parsed: ParsedFile {
            complete: true,
            ..ParsedFile::default()
        },
    };
    let key = CacheKey {
        codex_home: temp.path().join("home"),
        redact_content: false,
    };
    let mut hash_bytes = 0;
    let mut large_guard_bytes = 0;

    let contents = serialize_persistent_entry(
        &key,
        &source,
        &cached,
        MAX_PERSISTENT_ENTRY_BYTES,
        &mut hash_bytes,
        &mut large_guard_bytes,
    )
    .unwrap();
    let entry: PersistentFileEntry = serde_json::from_slice(&contents).unwrap();
    let PersistentSourceValidation::BoundedPrefixGuards { prefix_len, guards } =
        entry.source_validation.unwrap()
    else {
        panic!("large rollout entry must use bounded guards");
    };
    assert_eq!(prefix_len, MAX_PERSISTENT_PREFIX_HASH_BYTES + 1);
    assert_eq!(guards.len(), LARGE_PREFIX_GUARD_WINDOWS);
    assert_eq!(
        hash_bytes,
        (LARGE_PREFIX_GUARD_WINDOWS * LARGE_PREFIX_GUARD_WINDOW_BYTES) as u64
    );
    assert_eq!(large_guard_bytes, hash_bytes);
}

#[test]
fn malformed_large_guard_shape_is_rejected_before_source_io() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rollout.jsonl");
    let file = File::create(&source).unwrap();
    file.set_len(MAX_PERSISTENT_PREFIX_HASH_BYTES + 1).unwrap();
    let fingerprint = inspect_rollout_file(&source).unwrap().fingerprint;
    let mut build_hash_bytes = 0;
    let mut build_guard_bytes = 0;
    let mut validation = build_source_validation(
        &source,
        &fingerprint,
        &mut build_hash_bytes,
        &mut build_guard_bytes,
    )
    .unwrap();
    let PersistentSourceValidation::BoundedPrefixGuards { guards, .. } = &mut validation else {
        panic!("large rollout entry must use bounded guards");
    };
    guards[1].offset = guards[1].offset.saturating_add(1);

    let mut hash_bytes = 0;
    let mut large_guard_bytes = 0;
    assert!(!source_validation_matches(
        &source,
        fingerprint.len,
        &fingerprint,
        &validation,
        &mut hash_bytes,
        &mut large_guard_bytes,
    ));
    assert_eq!(hash_bytes, 0);
    assert_eq!(large_guard_bytes, 0);
}

#[test]
fn incomplete_in_memory_entry_is_reparsed_instead_of_hydrated_from_disk() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let source = sessions.join("rollout-incomplete.jsonl");
    let now = Utc::now();
    fs::write(
            &source,
            format!(
                "{{\"timestamp\":\"{}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"incomplete-thread\"}}}}\n",
                now.to_rfc3339()
            ),
        )
        .unwrap();
    let discovered = inspect_rollout_file(&source).unwrap();
    let key = CacheKey {
        codex_home: temp.path().to_owned(),
        redact_content: false,
    };
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        rollout_cache_dir: Some(temp.path().join("cache")),
        ..CollectConfig::default()
    };
    let (complete, _) = parse_stable_rollout_file(&discovered, &config);
    assert!(
        persist_file_entry(
            config.rollout_cache_dir.as_deref().unwrap(),
            &key,
            &source,
            &complete,
            MAX_PERSISTENT_ENTRY_BYTES,
        )
        .is_ok()
    );

    let mut cache = RolloutCache::new();
    cache.key = Some(key);
    cache.files.insert(
        source.clone(),
        CachedFile {
            fingerprint: discovered.fingerprint.clone(),
            parsed: ParsedFile::default(),
        },
    );
    cache.selected = vec![SelectedFile {
        path: source,
        fingerprint: discovered.fingerprint,
    }];
    cache.reduced = Some(ReducedRollouts::default());

    let dataset = cache.scan(&config, now).unwrap();

    assert_eq!(cache.last_refresh().disk_reused_files, 0);
    assert_eq!(cache.last_refresh().reparsed_files, 1);
    assert!(cache.last_refresh().rebuilt);
    assert_eq!(dataset.tasks[0].thread_id, "incomplete-thread");
}

#[test]
fn file_reordering_marks_only_threads_whose_internal_replay_order_changed() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.jsonl");
    let second = temp.path().join("second.jsonl");
    let unrelated = temp.path().join("unrelated.jsonl");
    for path in [&first, &second, &unrelated] {
        fs::write(path, b"{}\n").unwrap();
    }
    let mut cache = HashMap::new();
    for (path, owner_thread_id) in [
        (&first, "shared-thread"),
        (&second, "shared-thread"),
        (&unrelated, "other-thread"),
    ] {
        cache.insert(
            path.clone(),
            CachedFile {
                fingerprint: FileFingerprint::from_path_and_metadata(
                    path,
                    &path.metadata().unwrap(),
                )
                .unwrap(),
                parsed: ParsedFile {
                    owner_thread_id: Some(owner_thread_id.to_string()),
                    complete: true,
                    ..ParsedFile::default()
                },
            },
        );
    }
    let selected = |path: &Path| SelectedFile {
        path: path.to_owned(),
        fingerprint: cache.get(path).unwrap().fingerprint.clone(),
    };
    let previous = vec![selected(&first), selected(&unrelated), selected(&second)];
    let current = vec![selected(&second), selected(&unrelated), selected(&first)];
    let mut changed_thread_ids = HashSet::new();

    mark_threads_with_changed_file_order(&previous, &current, &cache, &mut changed_thread_ids);

    assert_eq!(
        changed_thread_ids,
        HashSet::from(["shared-thread".to_string()])
    );
}

#[test]
fn persistent_write_backoff_is_bounded() {
    let mut backoff = Duration::ZERO;
    for _ in 0..16 {
        backoff = next_write_backoff(backoff);
    }
    assert_eq!(backoff, PERSISTENT_WRITE_RETRY_MAX);
}

#[test]
fn future_timestamps_are_not_fresh_but_the_as_of_boundary_is() {
    let now = Utc::now();
    let grace = Duration::from_secs(30);

    assert!(timestamp_is_fresh(Some(now), now, grace));
    assert!(timestamp_is_fresh(
        Some(now - ChronoDuration::seconds(30)),
        now,
        grace
    ));
    assert!(!timestamp_is_fresh(
        Some(now - ChronoDuration::seconds(31)),
        now,
        grace
    ));
    assert!(!timestamp_is_fresh(
        Some(now + ChronoDuration::nanoseconds(1)),
        now,
        grace
    ));
}

#[test]
fn clock_rollback_hides_cached_future_rollout_evidence_until_catchup() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let task_at = base + ChronoDuration::seconds(10);
    let usage_at = base + ChronoDuration::seconds(11);
    let future_thread_at = base + ChronoDuration::seconds(12);
    let completion_at = base + ChronoDuration::seconds(13);
    let records = [
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": "clock-thread", "timestamp": base.to_rfc3339()}
        }),
        serde_json::json!({
            "timestamp": task_at.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "clock-turn"}
        }),
        serde_json::json!({
            "timestamp": usage_at.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 0,
                        "output_tokens": 0,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 10
                    }
                },
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"used_percent": 42.0, "window_minutes": 300}
                }
            }
        }),
        serde_json::json!({
            "timestamp": completion_at.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": "clock-turn"}
        }),
    ];
    let content = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(sessions.join("rollout-clock.jsonl"), content).unwrap();
    fs::write(
        sessions.join("rollout-future-thread.jsonl"),
        serde_json::json!({
            "timestamp": future_thread_at.to_rfc3339(),
            "type": "session_meta",
            "payload": {
                "id": "future-only-thread",
                "timestamp": future_thread_at.to_rfc3339()
            }
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        active_grace: Duration::from_secs(30),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let caught_up = cache
        .scan(&config, base + ChronoDuration::seconds(20))
        .unwrap();
    assert_eq!(caught_up.tasks.len(), 2);
    assert_eq!(caught_up.turns.len(), 1);
    assert_eq!(caught_up.calls.len(), 1);
    assert_eq!(caught_up.rate_observations.len(), 1);
    assert_eq!(
        caught_up
            .tasks
            .iter()
            .find(|task| task.thread_id == "clock-thread")
            .unwrap()
            .token_usage
            .total_tokens,
        10
    );

    let rolled_back = cache
        .scan(&config, base + ChronoDuration::seconds(5))
        .unwrap();
    assert_eq!(rolled_back.tasks.len(), 1);
    assert_eq!(rolled_back.tasks[0].thread_id, "clock-thread");
    assert_eq!(rolled_back.tasks[0].status, TaskStatus::Idle);
    assert_eq!(rolled_back.tasks[0].token_usage.total_tokens, 0);
    assert!(rolled_back.tasks[0].updated_at <= Some(base));
    assert!(rolled_back.turns.is_empty());
    assert!(rolled_back.calls.is_empty());
    assert!(rolled_back.rate_observations.is_empty());

    let at_task_boundary = cache.scan(&config, task_at).unwrap();
    assert_eq!(at_task_boundary.tasks.len(), 1);
    assert_eq!(at_task_boundary.tasks[0].status, TaskStatus::Running);
    assert_eq!(at_task_boundary.turns.len(), 1);
    assert_eq!(at_task_boundary.turns[0].started_at, Some(task_at));
    assert!(at_task_boundary.calls.is_empty());
    assert!(at_task_boundary.rate_observations.is_empty());

    let at_usage_boundary = cache.scan(&config, usage_at).unwrap();
    assert_eq!(at_usage_boundary.calls.len(), 1);
    assert_eq!(at_usage_boundary.rate_observations.len(), 1);
    assert_eq!(at_usage_boundary.tasks[0].token_usage.total_tokens, 10);

    let rolled_back_again = cache
        .scan(&config, base + ChronoDuration::seconds(5))
        .unwrap();
    assert!(rolled_back_again.calls.is_empty());
    assert!(rolled_back_again.rate_observations.is_empty());
    assert_eq!(rolled_back_again.tasks[0].token_usage.total_tokens, 0);

    assert!(
        cache
            .scan_if_changed(&config, base + ChronoDuration::seconds(6))
            .unwrap()
            .is_none()
    );
    let task_became_current = cache
        .scan_if_changed(&config, task_at)
        .unwrap()
        .expect("crossing a cached turn boundary must rematerialize");
    assert_eq!(task_became_current.turns.len(), 1);
    assert_eq!(task_became_current.tasks[0].status, TaskStatus::Running);

    let usage_became_current = cache
        .scan_if_changed(&config, usage_at)
        .unwrap()
        .expect("crossing a cached call boundary must rematerialize");
    assert_eq!(usage_became_current.calls.len(), 1);
    assert_eq!(usage_became_current.rate_observations.len(), 1);

    let future_thread_became_current = cache
        .scan_if_changed(&config, future_thread_at)
        .unwrap()
        .expect("crossing cached session metadata must rematerialize");
    assert_eq!(future_thread_became_current.tasks.len(), 2);

    let completion_became_current = cache
        .scan_if_changed(&config, completion_at)
        .unwrap()
        .expect("crossing a cached completion boundary must rematerialize");
    assert_eq!(
        completion_became_current
            .tasks
            .iter()
            .find(|task| task.thread_id == "clock-thread")
            .unwrap()
            .status,
        TaskStatus::Completed
    );
}

#[test]
fn future_session_metadata_does_not_decorate_an_older_visible_call() {
    let now = Utc::now();
    let future = now + ChronoDuration::hours(1);
    let thread_id = "mixed-clock-thread".to_string();
    let turn_id = "past-turn".to_string();
    let thread = ThreadBuilder {
        thread_id: thread_id.clone(),
        parent_thread_id: Some("future-parent".to_string()),
        parent_thread_rank: Some(ParentThreadRank::Direct),
        seen_archived_file: true,
        title: Some("future rollout title".to_string()),
        cwd: Some(PathBuf::from("/future/cwd")),
        source: Some("future-source".to_string()),
        created_at: Some(future),
        updated_at: Some(future),
        session_metadata_updated_at: Some(future),
        turns: HashMap::from([(
            turn_id.clone(),
            TurnBuilder {
                turn_id: turn_id.clone(),
                started_at: Some(now - ChronoDuration::minutes(1)),
                status: TurnStatus::Completed,
                ..TurnBuilder::default()
            },
        )]),
        ..ThreadBuilder::default()
    };
    let calls = vec![UsageCall {
        timestamp: now - ChronoDuration::seconds(30),
        thread_id: thread_id.clone(),
        turn_id: Some(turn_id),
        usage_event_id: None,
        usage_event_identity_exact: false,
        model: Some("gpt-5.6-sol".to_string()),
        service_tier: None,
        tokens: TokenUsage {
            total_tokens: 10,
            ..TokenUsage::default()
        },
        request_usage_exact: true,
    }];
    let config = CollectConfig::default();
    let titles = HashMap::from([(thread_id.clone(), "future index title".to_string())]);
    let mut current = RolloutDataset {
        calls: calls.clone(),
        ..RolloutDataset::default()
    };

    finish_dataset(
        &config,
        now,
        &HashMap::from([(thread_id.clone(), thread.clone())]),
        &titles,
        &mut current,
    );

    assert_eq!(current.tasks.len(), 1);
    let task = &current.tasks[0];
    assert_eq!(task.title, "Untitled task");
    assert!(task.parent_thread_id.is_none());
    assert!(task.cwd.is_none());
    assert!(task.source.is_none());
    assert!(!task.archived);
    assert!(task.created_at.is_none());
    assert!(task.updated_at.is_some_and(|timestamp| timestamp <= now));
    assert_eq!(task.token_usage.total_tokens, 10);

    let mut caught_up = RolloutDataset {
        calls,
        ..RolloutDataset::default()
    };
    finish_dataset(
        &config,
        future,
        &HashMap::from([(thread_id, thread)]),
        &titles,
        &mut caught_up,
    );
    assert_eq!(caught_up.tasks[0].title, "future index title");
    assert_eq!(
        caught_up.tasks[0].parent_thread_id.as_deref(),
        Some("future-parent")
    );
    assert_eq!(
        caught_up.tasks[0].cwd.as_deref(),
        Some(Path::new("/future/cwd"))
    );
    assert!(caught_up.tasks[0].archived);
}

#[test]
fn as_of_replay_excludes_future_state_and_counter_resets_until_each_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let future_meta = base + ChronoDuration::seconds(20);
    let future_message = base + ChronoDuration::seconds(21);
    let future_settings = base + ChronoDuration::seconds(22);
    let future_counter = base + ChronoDuration::seconds(23);
    let future_activity = base + ChronoDuration::seconds(24);
    let effective_completion = base + ChronoDuration::seconds(30);
    let records = [
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "session_meta",
            "payload": {
                "id": "as-of-thread",
                "timestamp": base.to_rfc3339(),
                "cwd": "/past/cwd"
            }
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "turn"}
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "turn_context",
            "payload": {"turn_id": "turn", "model": "past-model", "effort": "low"}
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "spawn_agent",
                "call_id": "future-activity-call",
                "internal_chat_message_metadata_passthrough": {"turn_id": "turn"}
            }
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(2)).to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 100
                }}
            }
        }),
        // The record timestamp predates the payload's effective timestamp.
        // Neither half may make the session metadata visible early.
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(3)).to_rfc3339(),
            "type": "session_meta",
            "payload": {
                "id": "as-of-thread",
                "timestamp": future_meta.to_rfc3339(),
                "thread_source": "subagent",
                "parent_thread_id": "future-parent"
            }
        }),
        serde_json::json!({
            "timestamp": future_message.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "turn_id": "turn",
                "message": "Future task title"
            }
        }),
        serde_json::json!({
            "timestamp": future_settings.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "fast"}
            }
        }),
        serde_json::json!({
            "timestamp": future_settings.to_rfc3339(),
            "type": "turn_context",
            "payload": {"turn_id": "turn", "model": "future-model", "effort": "high"}
        }),
        serde_json::json!({
            "timestamp": future_counter.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 10
                }}
            }
        }),
        // This record is appended after the future reset but has a corrected
        // wall-clock timestamp. It must not use the hidden reset as evidence.
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(3)).to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 20,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 20
                }}
            }
        }),
        // The semantic occurrence precedes the record timestamp. The activity
        // is not evidence until the record itself exists.
        serde_json::json!({
            "timestamp": future_activity.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "kind": "started",
                "event_id": "future-activity-call",
                "agent_thread_id": "future-child",
                "occurred_at_ms": (base + ChronoDuration::seconds(2)).timestamp_millis()
            }
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(4)).to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn",
                "completed_at": effective_completion.to_rfc3339()
            }
        }),
    ];
    fs::write(
        sessions.join("rollout-as-of.jsonl"),
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        active_grace: Duration::from_secs(3_600),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let current = cache
        .scan(&config, base + ChronoDuration::seconds(5))
        .unwrap();
    assert_eq!(current.tasks.len(), 1);
    assert_eq!(current.tasks[0].title, "Untitled task");
    assert_eq!(
        current.tasks[0].cwd.as_deref(),
        Some(Path::new("/past/cwd"))
    );
    assert!(current.tasks[0].parent_thread_id.is_none());
    assert_eq!(current.tasks[0].status, TaskStatus::Running);
    assert_eq!(current.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(current.calls.len(), 1);
    assert_eq!(current.stats.ambiguous_token_resets, 1);
    assert_eq!(current.turns[0].model.as_deref(), Some("past-model"));
    assert_eq!(current.turns[0].reasoning_effort.as_deref(), Some("low"));
    assert!(current.turns[0].service_tier.is_none());
    assert!(current.turns[0].message_preview.is_none());
    assert!(current.turns[0].completed_at.is_none());
    assert!(current.agent_interactions.is_empty());
    assert_eq!(cache.as_of_reduce_count, 1);

    let forced_same_interval = cache
        .scan(&config, base + ChronoDuration::seconds(6))
        .unwrap();
    assert_eq!(forced_same_interval.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(
        cache.as_of_reduce_count, 1,
        "forced materialization must reuse the reduced as-of interval"
    );

    let metadata_visible = cache
        .scan_if_changed(&config, future_meta)
        .unwrap()
        .expect("payload timestamp must schedule session metadata catch-up");
    assert_eq!(cache.as_of_reduce_count, 2);
    assert_eq!(
        metadata_visible.tasks[0].parent_thread_id.as_deref(),
        Some("future-parent")
    );
    assert_eq!(metadata_visible.tasks[0].title, "Untitled task");

    let message_visible = cache
        .scan_if_changed(&config, future_message)
        .unwrap()
        .expect("future message must materialize at its own boundary");
    assert_eq!(message_visible.tasks[0].title, "Future task title");
    assert_eq!(
        message_visible.turns[0].message_preview.as_deref(),
        Some("Future task title")
    );

    let settings_visible = cache
        .scan_if_changed(&config, future_settings)
        .unwrap()
        .expect("future settings/context must materialize at their boundary");
    assert_eq!(
        settings_visible.turns[0].model.as_deref(),
        Some("future-model")
    );
    assert_eq!(
        settings_visible.turns[0].reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        settings_visible.turns[0].service_tier.as_deref(),
        Some("fast")
    );

    let activity_visible = cache
        .scan_if_changed(&config, future_activity)
        .unwrap()
        .expect("record timestamp must schedule the agent activity boundary");
    assert_eq!(activity_visible.agent_interactions.len(), 1);
    assert_eq!(
        activity_visible.agent_interactions[0]
            .occurred_at
            .unwrap()
            .timestamp_millis(),
        (base + ChronoDuration::seconds(2)).timestamp_millis()
    );

    let completed = cache
        .scan_if_changed(&config, effective_completion)
        .unwrap()
        .expect("payload completion time must schedule the completion boundary");
    assert_eq!(completed.tasks[0].status, TaskStatus::Completed);

    let rolled_back = cache
        .scan(&config, base + ChronoDuration::seconds(5))
        .unwrap();
    assert_eq!(rolled_back.tasks[0].title, "Untitled task");
    assert_eq!(rolled_back.tasks[0].status, TaskStatus::Running);
    assert_eq!(rolled_back.tasks[0].token_usage.total_tokens, 100);
}

#[test]
fn future_quota_only_sample_does_not_break_the_visible_token_counter() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let future = base + ChronoDuration::seconds(20);
    let records = [
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": "quota-only-thread", "timestamp": base.to_rfc3339()}
        }),
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "turn"}
        }),
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": {
                "input_tokens": 100,
                "cached_input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0,
                "total_tokens": 100
            }}}
        }),
        serde_json::json!({
            "timestamp": future.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"used_percent": 42.0, "window_minutes": 300}
                }
            }
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {
                    "input_tokens": 110,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 110
                },
                "last_token_usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 10
                }
            }}
        }),
    ];
    fs::write(
        sessions.join("rollout-quota-only.jsonl"),
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let current = cache
        .scan(&config, base + ChronoDuration::seconds(2))
        .unwrap();
    assert_eq!(current.calls.len(), 2);
    assert_eq!(current.tasks[0].token_usage.total_tokens, 110);
    assert_eq!(current.stats.ambiguous_token_resets, 0);
    assert!(current.rate_observations.is_empty());

    let caught_up = cache.scan_if_changed(&config, future).unwrap().unwrap();
    assert_eq!(caught_up.tasks[0].token_usage.total_tokens, 110);
    assert_eq!(caught_up.stats.ambiguous_token_resets, 0);
    assert_eq!(caught_up.rate_observations.len(), 1);
}

#[test]
fn redacted_nonprojected_activity_keeps_each_as_of_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let middle = base + ChronoDuration::seconds(10);
    let future = base + ChronoDuration::seconds(20);
    let records = [
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": "redacted-activity", "timestamp": base.to_rfc3339()}
        }),
        serde_json::json!({
            "timestamp": middle.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "redacted input"}
        }),
        serde_json::json!({
            "timestamp": future.to_rfc3339(),
            "type": "response_item",
            "payload": {"type": "message", "role": "assistant", "content": []}
        }),
    ];
    fs::write(
        sessions.join("rollout-redacted-activity.jsonl"),
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        redact_content: true,
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let initial = cache.scan(&config, base).unwrap();
    assert_eq!(initial.tasks[0].updated_at, Some(base));

    let middle_view = cache
        .scan_if_changed(&config, middle)
        .unwrap()
        .expect("redacted activity must preserve its intermediate boundary");
    assert_eq!(middle_view.tasks[0].updated_at, Some(middle));

    let future_view = cache
        .scan_if_changed(&config, future)
        .unwrap()
        .expect("nonprojected response activity must preserve its boundary");
    assert_eq!(future_view.tasks[0].updated_at, Some(future));
}

#[test]
fn future_file_overlap_does_not_poison_the_current_replay_plan() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let second_file_at = base + ChronoDuration::seconds(20);
    let first_file_future = base + ChronoDuration::seconds(30);
    fs::write(
        sessions.join("rollout-a.jsonl"),
        [
            serde_json::json!({
                "timestamp": base.to_rfc3339(),
                "type": "session_meta",
                "payload": {"id": "overlap-thread", "timestamp": base.to_rfc3339()}
            }),
            serde_json::json!({
                "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 100
                }}}
            }),
            serde_json::json!({
                "timestamp": first_file_future.to_rfc3339(),
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": []}
            }),
        ]
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n",
    )
    .unwrap();
    fs::write(
        sessions.join("rollout-b.jsonl"),
        [
            serde_json::json!({
                "timestamp": second_file_at.to_rfc3339(),
                "type": "session_meta",
                "payload": {
                    "id": "overlap-thread",
                    "timestamp": second_file_at.to_rfc3339()
                }
            }),
            serde_json::json!({
                "timestamp": (second_file_at + ChronoDuration::seconds(1)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {
                    "input_tokens": 120,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 120
                }}}
            }),
        ]
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let current = cache
        .scan(&config, base + ChronoDuration::seconds(2))
        .unwrap();

    assert_eq!(current.tasks[0].token_usage.total_tokens, 100);
    assert_eq!(current.calls.len(), 1);
    assert_eq!(current.stats.ambiguous_token_resets, 0);
    assert!(
        current
            .warnings
            .iter()
            .all(|warning| { !warning.contains("non-overlapping content timestamp order") })
    );
}

#[test]
fn foreign_settings_timeline_restores_the_latest_visible_inheritance() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let base = Utc::now() - ChronoDuration::seconds(1);
    let future = base + ChronoDuration::seconds(20);
    let records = [
        serde_json::json!({
            "timestamp": base.to_rfc3339(),
            "type": "session_meta",
            "payload": {
                "id": "settings-child",
                "timestamp": base.to_rfc3339(),
                "source": {"subagent": {"thread_spawn": {
                    "parent_thread_id": "settings-parent",
                    "agent_role": null
                }}}
            }
        }),
        serde_json::json!({
            "timestamp": (base - ChronoDuration::seconds(2)).to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": "settings-parent"}
        }),
        serde_json::json!({
            "timestamp": (base - ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "thread_settings_applied", "thread_settings": {
                "model": "gpt-child",
                "service_tier": null
            }}
        }),
        serde_json::json!({
            "timestamp": future.to_rfc3339(),
            "type": "event_msg",
            "payload": {"type": "thread_settings_applied", "thread_settings": {
                "model": "gpt-child",
                "service_tier": "fast"
            }}
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": "child-turn",
                "started_at": (base + ChronoDuration::seconds(1)).to_rfc3339()
            }
        }),
        serde_json::json!({
            "timestamp": (base + ChronoDuration::seconds(1)).to_rfc3339(),
            "type": "turn_context",
            "payload": {"turn_id": "child-turn", "model": "gpt-child"}
        }),
    ];
    fs::write(
        sessions.join("rollout-foreign-settings.jsonl"),
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let current = cache
        .scan(&config, base + ChronoDuration::seconds(2))
        .unwrap();
    assert_eq!(current.turns[0].service_tier.as_deref(), Some("default"));

    let caught_up = cache.scan_if_changed(&config, future).unwrap().unwrap();
    assert_eq!(caught_up.turns[0].service_tier.as_deref(), Some("fast"));
}

#[test]
fn cached_session_titles_rematerialize_at_updated_at_without_file_changes() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let now = Utc::now() - ChronoDuration::seconds(1);
    let future = now + ChronoDuration::seconds(10);
    fs::write(
        sessions.join("rollout-title.jsonl"),
        serde_json::json!({
            "timestamp": now.to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": "title-thread", "timestamp": now.to_rfc3339()}
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("session_index.jsonl"),
        format!(
            "{{\"id\":\"title-thread\",\"thread_name\":\"Current\",\"updated_at\":\"{}\"}}\n{{\"id\":\"title-thread\",\"thread_name\":\"Future\",\"updated_at\":\"{}\"}}\n",
            (now - ChronoDuration::seconds(1)).to_rfc3339(),
            future.to_rfc3339(),
        ),
    )
    .unwrap();
    let config = CollectConfig {
        codex_home: temp.path().to_owned(),
        ..CollectConfig::default()
    };
    let mut cache = RolloutCache::new();

    let current = cache.scan_if_changed(&config, now).unwrap().unwrap();
    assert_eq!(current.tasks[0].title, "Current");
    assert!(
        cache
            .scan_if_changed(&config, future - ChronoDuration::seconds(1))
            .unwrap()
            .is_none()
    );
    let caught_up = cache
        .scan_if_changed(&config, future)
        .unwrap()
        .expect("session title updatedAt must trigger a cached rematerialization");
    assert_eq!(caught_up.tasks[0].title, "Future");
    assert_eq!(cache.last_refresh().session_index_reads, 1);
    assert_eq!(cache.last_refresh().reparsed_files, 0);
}
