use super::*;

#[test]
fn pruning_removes_entries_until_within_bounds() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("a.json");
    let second = temp.path().join("b.json");
    let third = temp.path().join("c.json");
    fs::write(&first, b"one").unwrap();
    fs::write(&second, b"two").unwrap();
    fs::write(&third, b"three").unwrap();

    let pruned = prune_cache_directory(temp.path(), None, 1, u64::MAX, SystemTime::UNIX_EPOCH);

    assert_eq!(pruned.entries, 2);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn pruning_removes_only_stale_cache_temporary_files() {
    let temp = tempfile::tempdir().unwrap();
    let stale = temp.path().join(".0123456789abcdef.json.42.7.tmp");
    let unrelated = temp.path().join("notes.tmp");
    fs::write(&stale, b"partial").unwrap();
    fs::write(&unrelated, b"keep").unwrap();

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
    fs::write(&first, [0_u8; 8]).unwrap();
    fs::write(&target, [0_u8; 8]).unwrap();
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
    let fingerprint = FileFingerprint::from_metadata(&source.metadata().unwrap()).unwrap();
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
    let fingerprint = FileFingerprint::from_metadata(&source.metadata().unwrap()).unwrap();
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
    let after_shrink = cache.persist_dirty_files_with_limit(&config, &key, 1024);
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

    assert!(matches!(
        load_persistent_file(&cache_root, &key, &discovered),
        PersistentLoad::Miss
    ));
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
fn persistent_write_backoff_is_bounded() {
    let mut backoff = Duration::ZERO;
    for _ in 0..16 {
        backoff = next_write_backoff(backoff);
    }
    assert_eq!(backoff, PERSISTENT_WRITE_RETRY_MAX);
}
