use super::*;
use crate::history::HISTORY_METRIC_REVISION;
use crate::source_history::{
    CompleteFactBatch, FactBatchId, FactBatchKind, FactCursor, FactDigestBinding,
    SessionUsageMetrics, SourceSessionDigest, SourceSessionDigestRecord, UsageEventFact,
    UsageEventFactChange, UsageEventFactRecord,
};
use crate::source_model::{ObservedProjectKey, SessionReplicaKey, ThreadId};

struct ReplicaFacts {
    source: NodeId,
    binding: Option<SourceHistoryRemoteBinding>,
    replica: SessionReplicaKey,
    records: Vec<UsageEventFactRecord>,
    proof: Vec<FactDigestBinding>,
}

fn metrics(total: u64, calls: u64) -> SessionUsageMetrics {
    SessionUsageMetrics {
        token_usage: TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..Default::default()
        },
        estimated_cost_units: u128::from(total),
        api_long_context_extra_cost_units: Some(0),
        call_count: calls,
        metric_revision: HISTORY_METRIC_REVISION,
        estimator_revision: HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
        ..Default::default()
    }
}

fn fixture_replica(
    source: NodeId,
    project: char,
    starts_at: DateTime<Utc>,
    remote: bool,
) -> (ReplicaFacts, LocalHalfHourBucket, SourceSessionDigest) {
    let thread: ThreadId = "thread-1".parse().unwrap();
    let replica = SessionReplicaKey::new(source.clone(), thread.clone());
    let project: ObservedProjectKey =
        format!("opk-hmac-sha256-v1-{}", project.to_string().repeat(64))
            .parse()
            .unwrap();
    let mut records = Vec::new();
    for (event, minutes, tokens) in if remote {
        vec![("shared", 1, 10), ("remote-unique", 2, 30)]
    } else {
        vec![("shared", 1, 10), ("local-unique", 2, 20)]
    } {
        let m = metrics(tokens, 1);
        let fact = UsageEventFact::new(
            replica.clone(),
            event.parse().unwrap(),
            starts_at + ChronoDuration::minutes(minutes),
            project.clone(),
            Some("turn".into()),
            None,
            Some(thread.clone()),
            thread.clone(),
            Some("turn".into()),
            Some("gpt-5.6-sol".into()),
            Some("standard".into()),
            m.token_usage,
            true,
            true,
            m,
        )
        .unwrap();
        records.push(UsageEventFactRecord::upsert(1, fact).unwrap());
    }
    let total = if remote { 40 } else { 30 };
    let mut bucket = tui_runtime_test_bucket(starts_at, total);
    bucket.call_count = records.len() as u64;
    bucket.project_groups[0].project_id = Some(project.as_str().into());
    bucket.project_groups[0].turn_id = Some("turn".into());
    bucket.project_groups[0].session_thread_id = Some("thread-1".into());
    bucket.project_groups[0].session_turn_id = Some("turn".into());
    bucket.project_groups[0].api_long_context_extra_cost_units = Some(0);
    bucket.project_groups[0].call_count = records.len() as u64;
    let range_start = starts_at
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let range_end = range_start + ChronoDuration::days(1);
    let facts = records
        .iter()
        .map(|record| match record.change() {
            UsageEventFactChange::Upsert(fact) => fact.as_ref(),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    let (fingerprint, project_fingerprint) =
        crate::source_export::canonical_fact_fingerprints_for_test(
            &replica,
            range_start,
            range_end,
            &facts,
        )
        .unwrap();
    let digest = SourceSessionDigest::new(
        replica.clone(),
        range_start,
        range_end,
        range_end,
        fingerprint,
        project_fingerprint,
        records.len() as u64,
        true,
        true,
        vec![project],
        metrics(total, records.len() as u64),
    )
    .unwrap();
    let binding = remote.then(|| {
        SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source.clone(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            crate::remote_agent::current_revisions(),
        )
        .unwrap()
    });
    (
        ReplicaFacts {
            source,
            binding,
            replica,
            records,
            proof: vec![FactDigestBinding::from_digest(&digest).unwrap()],
        },
        bucket,
        digest,
    )
}

fn publish(
    store: &TuiHistoryStore,
    replica: &ReplicaFacts,
    kind: FactBatchKind,
    with_proof: bool,
) -> crate::source_history::FactActivationReport {
    try_publish(store, replica, kind, with_proof).unwrap()
}

fn try_publish(
    store: &TuiHistoryStore,
    replica: &ReplicaFacts,
    kind: FactBatchKind,
    with_proof: bool,
) -> io::Result<crate::source_history::FactActivationReport> {
    let TuiHistoryBackend::Runtime(runtime) = &store.backend else {
        unreachable!()
    };
    let active = runtime.source_history().load_active_fact_set(
        &replica.source,
        runtime.redaction_profile(),
        replica.replica.thread_id(),
    )?;
    let batch = CompleteFactBatch {
        batch_id: FactBatchId::generate().unwrap(),
        kind,
        replica: replica.replica.clone(),
        expected_active_version: active.as_ref().map(|active| active.version.clone()),
        remote_binding: replica.binding.clone(),
        validated_digests: if with_proof {
            replica.proof.clone()
        } else {
            vec![]
        },
        activate_cursor: active.as_ref().map_or(
            FactCursor::new(1, replica.records.len() as u64).unwrap(),
            |active| active.cursor,
        ),
        completed_at: Utc.with_ymd_and_hms(2026, 8, 31, 0, 0, 0).unwrap(),
        changes: if kind == FactBatchKind::Snapshot {
            replica.records.clone()
        } else {
            vec![]
        },
    };
    let manifest = match runtime.ownership().load_manifest().unwrap() {
        OwnershipManifestStatus::Initialized(manifest) => manifest,
        _ => unreachable!(),
    };
    let lease = runtime.ownership().acquire_writer_lease().unwrap();
    let authority = runtime
        .ownership()
        .authorize_v2_write(&lease, &manifest)
        .unwrap();
    runtime
        .source_history()
        .writer(&authority)
        .unwrap()
        .stage_and_activate_complete_fact_batch(
            &replica.source,
            runtime.redaction_profile(),
            &batch,
        )
}

#[test]
fn v2_projection_cache_invalidates_for_external_local_remote_facts_and_proof_only_publication() {
    let directory = tempfile::tempdir().unwrap();
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let mut runtime = HistoryRuntime::new(
        directory.path().join("state/history-v1"),
        &codex_home,
        false,
    )
    .unwrap();
    let profile_lease = acquire_runtime_profile_lease(&runtime).unwrap();
    let active = runtime.ensure_v2_active().unwrap();
    let starts_at = Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap();
    let (local, local_bucket, local_digest) = fixture_replica(
        runtime.source_identity().node_id().clone(),
        'a',
        starts_at,
        false,
    );
    let (remote, remote_bucket, remote_digest) = fixture_replica(
        "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
        'b',
        starts_at,
        true,
    );
    {
        let lease = runtime.ownership().acquire_writer_lease().unwrap();
        let authority = runtime
            .ownership()
            .authorize_v2_write(&lease, &active)
            .unwrap();
        let writer = runtime.source_history().writer(&authority).unwrap();
        for (replica, kind, label) in [
            (&local, SourceKind::Local, "local"),
            (&remote, SourceKind::Ssh, "remote"),
        ] {
            writer
                .save_source_metadata(
                    &SourceMetadata::new_with_redaction_profile(
                        replica.source.clone(),
                        kind,
                        label,
                        runtime.redaction_profile(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        writer
            .record_source_bucket_changes(
                &local.source,
                runtime.redaction_profile(),
                &[SourceBucketRecord::upsert(1, local_bucket).unwrap()],
            )
            .unwrap();
        writer
            .record_source_session_digest_changes(
                &local.source,
                runtime.redaction_profile(),
                &[SourceSessionDigestRecord::upsert(1, local_digest).unwrap()],
            )
            .unwrap();
        let generation: SourceHistoryRemoteGenerationId =
            "ingest-gen-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap();
        let binding = remote.binding.as_ref().unwrap();
        writer
            .ensure_remote_history_generation(
                &remote.source,
                runtime.redaction_profile(),
                &generation,
                binding,
            )
            .unwrap();
        writer
            .apply_remote_history_generation_page(
                &remote.source,
                runtime.redaction_profile(),
                &generation,
                binding,
                &[SourceBucketRecord::upsert(1, remote_bucket).unwrap()],
                &[SourceSessionDigestRecord::upsert(1, remote_digest).unwrap()],
                &[],
            )
            .unwrap();
        writer
            .activate_remote_history_generation(
                &remote.source,
                runtime.redaction_profile(),
                None,
                &generation,
                binding,
                starts_at + ChronoDuration::minutes(20),
            )
            .unwrap();
    }
    let mut store = TuiHistoryStore::runtime(runtime, Some(profile_lease), vec![]);
    let selection = HistorySourceSelection::AllIncluded;
    let since = history_view_since(starts_at + ChronoDuration::minutes(20));
    let initial = store.load_since_with_staged_selected(&selection, since);
    assert!(initial.history.warnings.iter().any(
        |warning| warning == crate::history_query::DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING
    ));
    assert!(store.projection_cache_valid(&selection, since, false));
    for (replica, kind, proof) in [
        (&local, FactBatchKind::Snapshot, false),
        (&remote, FactBatchKind::Snapshot, false),
        (&local, FactBatchKind::Delta, true),
        (&remote, FactBatchKind::Delta, true),
    ] {
        let before = store.projection_cache.as_ref().unwrap().revision.clone();
        assert!(publish(&store, replica, kind, proof).activated);
        let after = store.projection_revision(&selection).unwrap().unwrap();
        assert_eq!(after.facts_revision, before.facts_revision + 1);
        assert_eq!(
            before.local_observation_revision,
            after.local_observation_revision
        );
        assert_eq!(
            before.other_local_observation_revision,
            after.other_local_observation_revision
        );
        assert_eq!(
            before.sources, after.sources,
            "aggregate/source metadata are unchanged throughout facts follow-up"
        );
        // Test the publication revision, independently of machine speed/TTL.
        store.projection_cache.as_mut().unwrap().loaded_at = Instant::now();
        assert!(
            !store.projection_cache_valid(&selection, since, false),
            "external facts/proof publication must invalidate the existing TUI cache without a UI completion notification"
        );
        assert!(
            !before.same_query_inputs_except_local_revision(&after),
            "no-op local maintenance must not rebase across facts publication"
        );
        let reloaded = store
            .reload_since_if_stale_with_staged_selected(&selection, since)
            .unwrap();
        assert!(reloaded.query_error.is_none());
        assert!(store.projection_cache_valid(&selection, since, false));
    }
    let completed = store.clone_cached_projection().unwrap();
    assert_eq!(
        completed.history.half_hour_buckets[0]
            .token_usage
            .total_tokens,
        60
    );
    assert!(
        initial.history.half_hour_buckets[0]
            .token_usage
            .total_tokens
            < 60,
        "facts union adds the unique events from both fixed physical aggregates"
    );
    assert!(!completed.history.warnings.iter().any(
        |warning| warning == crate::history_query::DUPLICATE_SESSION_DEDUP_UNAVAILABLE_WARNING
    ));
    assert_ne!(
        initial.history, completed.history,
        "proof-only activation must change the fresh query's dedup evidence"
    );
    let TuiHistoryBackend::Runtime(runtime) = &mut store.backend else {
        unreachable!()
    };
    let fresh = runtime
        .load_unified_history_since_with_staged_selected(&selection, since)
        .unwrap()
        .history;
    assert_eq!(completed.history, fresh);
    for replica in [&local, &remote] {
        let before = store.projection_revision(&selection).unwrap().unwrap();
        assert!(!publish(&store, replica, FactBatchKind::Delta, true).activated);
        assert_eq!(
            before,
            store.projection_revision(&selection).unwrap().unwrap(),
            "empty same-cursor same-proof delta is a no-op"
        );
        store.projection_cache.as_mut().unwrap().loaded_at = Instant::now();
        assert!(store.projection_cache_valid(&selection, since, false));
    }
}

#[test]
fn sqlite_fact_projection_stamp_and_active_proof_roll_back_together_then_retry() {
    for remote in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let mut runtime = HistoryRuntime::new(
            directory.path().join("state/history-v1"),
            &codex_home,
            false,
        )
        .unwrap();
        let active = runtime.ensure_v2_active().unwrap();
        let source = if remote {
            "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap()
        } else {
            runtime.source_identity().node_id().clone()
        };
        let (replica, _, _) = fixture_replica(
            source.clone(),
            'a',
            Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap(),
            remote,
        );
        {
            let lease = runtime.ownership().acquire_writer_lease().unwrap();
            let authority = runtime
                .ownership()
                .authorize_v2_write(&lease, &active)
                .unwrap();
            runtime
                .source_history()
                .writer(&authority)
                .unwrap()
                .save_source_metadata(
                    &SourceMetadata::new_with_redaction_profile(
                        source.clone(),
                        if remote {
                            SourceKind::Ssh
                        } else {
                            SourceKind::Local
                        },
                        "controlled",
                        runtime.redaction_profile(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let store = TuiHistoryStore::runtime(runtime, None, vec![]);
        assert!(publish(&store, &replica, FactBatchKind::Snapshot, false).activated);
        let TuiHistoryBackend::Runtime(runtime) = &store.backend else {
            unreachable!()
        };
        let source_history = runtime.source_history();
        let redaction = runtime.redaction_profile();
        let database = source_history.sqlite_database().unwrap();
        let before = source_history
            .load_active_fact_set(&source, redaction, replica.replica.thread_id())
            .unwrap()
            .unwrap();
        let revision = source_history.load_facts_projection_revision().unwrap();
        assert_eq!(revision, 1);
        database
            .write(|connection| {
                // Keep the injected trigger inside one validated connection.
                // A persisted trigger is correctly rejected by schema safety.
                connection
                    .execute_batch(
                        "CREATE TRIGGER fail_fact_projection_stamp BEFORE INSERT ON history_state
                 WHEN NEW.state_key='facts-query-publication.json'
                 BEGIN SELECT RAISE(FAIL,'reject facts projection stamp'); END",
                    )
                    .map_err(crate::source_history::database::sql_error)?;
                let failed = try_publish(&store, &replica, FactBatchKind::Delta, true).unwrap_err();
                assert!(
                    failed.to_string().contains("reject facts projection stamp"),
                    "{failed}"
                );
                assert_eq!(source_history.load_facts_projection_revision()?, revision);
                let after_failure = source_history
                    .load_active_fact_set(&source, redaction, replica.replica.thread_id())?
                    .unwrap();
                assert_eq!(
                    after_failure, before,
                    "a failing stamp write must roll back the earlier active proof/cursor switch"
                );
                connection
                    .execute_batch("DROP TRIGGER fail_fact_projection_stamp")
                    .map_err(crate::source_history::database::sql_error)
            })
            .unwrap();
        assert_eq!(
            source_history.load_facts_projection_revision().unwrap(),
            revision
        );
        assert!(publish(&store, &replica, FactBatchKind::Delta, true).activated);
        assert_eq!(
            source_history.load_facts_projection_revision().unwrap(),
            revision + 1
        );
        let recovered = source_history
            .load_active_fact_set(&source, redaction, replica.replica.thread_id())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.records, before.records);
        assert_eq!(recovered.cursor, before.cursor);
        assert_eq!(recovered.version.validated_digests(), replica.proof);
        assert!(!publish(&store, &replica, FactBatchKind::Delta, true).activated);
        assert_eq!(
            source_history.load_facts_projection_revision().unwrap(),
            revision + 1
        );
    }
}
