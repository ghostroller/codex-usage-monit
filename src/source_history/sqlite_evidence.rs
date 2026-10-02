//! Typed session evidence and immutable fact publication in SQLite.
//!
//! Replica namespaces remain physical (source + thread). A staged generation
//! is invisible until the active manifest, digest proof and cursor are published
//! together by a short compare-and-swap transaction.
use super::*;

fn generation_namespace(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source_id: &NodeId,
    redaction: RedactionProfile,
    shard_key: &ThreadShardKey,
    generation: &FactBatchId,
) -> io::Result<String> {
    database.namespace(
        &store
            .source_facts_directory(source_id, redaction)
            .join(shard_key.as_str())
            .join(generation.as_str()),
    )
}

fn staged_key(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source_id: &NodeId,
    redaction: RedactionProfile,
    batch_id: &FactBatchId,
    file: &str,
) -> io::Result<String> {
    database.namespace(
        &store
            .source_fact_staging_directory(source_id, redaction)
            .join(batch_id.as_str())
            .join(file),
    )
}

fn manifest_key(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source_id: &NodeId,
    redaction: RedactionProfile,
    shard_key: &ThreadShardKey,
) -> io::Result<String> {
    database.namespace(&fact_manifest_path(
        &store.source_fact_manifests_directory(source_id, redaction),
        shard_key,
    ))
}

fn record_days(records: &[UsageEventFactRecord]) -> Vec<NaiveDate> {
    records
        .iter()
        .map(|record| record.occurred_at().date_naive())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_proof(
    replica: &SessionReplicaKey,
    records: &[UsageEventFactRecord],
    bindings: &[FactDigestBinding],
    retained_since: Option<DateTime<Utc>>,
) -> io::Result<()> {
    let facts = records
        .iter()
        .filter_map(|record| match record.change() {
            UsageEventFactChange::Upsert(fact) => Some(fact.as_ref()),
            UsageEventFactChange::Tombstone => None,
        })
        .collect::<Vec<_>>();
    crate::source_export::validate_fact_digest_bindings_against_facts(
        replica,
        &facts,
        bindings,
        retained_since,
    )
}

fn read_descriptor(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
    batch_id: &FactBatchId,
) -> io::Result<StagedFactBatch> {
    let path = store
        .source_fact_staging_directory(source_id, redaction)
        .join(batch_id.as_str())
        .join(STAGED_BATCH_FILE);
    let key = database.namespace(&path)?;
    let descriptor = sqlite_state_bounded(connection, &key, MAX_FACT_MANIFEST_BYTES, None)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "staged fact batch is missing"))?;
    validate_staged_batch(
        descriptor,
        &path,
        &store.profile_id,
        source_id,
        redaction,
        batch_id,
    )
}

fn ensure_sql_fact_cap(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<()> {
    let root = format!(
        "{}/",
        database.namespace(
            &store
                .source_directory(source_id)
                .join(redaction.directory_name())
        )?
    );
    // The old entry cap counted files/directories, not individual events.
    // Count SQL generation namespaces and state descriptors equivalently;
    // per-generation record limits are enforced separately.
    let (record_bytes, generation_count): (i64, i64) = connection.query_row(
        "SELECT COALESCE(sum(length(payload)),0),count(DISTINCT namespace) FROM history_records WHERE substr(namespace,1,length(?1))=?1 AND substr(namespace,length(?1)+1,6)='facts/'", [&root], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(database::sql_error)?;
    let (state_bytes, state_count): (i64, i64) = connection.query_row(
        "SELECT COALESCE(sum(length(payload)),0),count(*) FROM history_state WHERE substr(state_key,1,length(?1))=?1 AND (substr(state_key,length(?1)+1,15)='fact-manifests/' OR substr(state_key,length(?1)+1,13)='fact-staging/')", [&root], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(database::sql_error)?;
    let usage = FactNamespaceUsage {
        bytes: u64::try_from(
            record_bytes
                .checked_add(state_bytes)
                .ok_or_else(|| invalid_data("fact namespace size overflowed"))?,
        )
        .map_err(|_| invalid_data("invalid SQL fact namespace size"))?,
        entries: u64::try_from(
            generation_count
                .checked_add(state_count)
                .ok_or_else(|| invalid_data("fact namespace entry count overflowed"))?,
        )
        .map_err(|_| invalid_data("invalid SQL fact namespace entry count"))?,
    };
    validate_fact_namespace_usage(usage, MAX_FACT_NAMESPACE_BYTES, MAX_FACT_NAMESPACE_ENTRIES)
}

impl SourceHistoryStore {
    pub(super) fn sqlite_record_digest_changes(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        directory: &Path,
        records: &[SourceSessionDigestRecord],
    ) -> io::Result<SourceHistoryWriteReport> {
        let database = self.sqlite_database().expect("SQLite backend");
        let additions = group_digest_records_by_day(source_id, records)?;
        if additions.is_empty() {
            return Ok(SourceHistoryWriteReport::default());
        }
        database.write(|connection| {
            let namespace = database.namespace(directory)?;
            let mut report = SourceHistoryWriteReport::default();
            for (_, additions) in additions {
                let mut changed = false;
                for incoming in additions {
                    let key = sqlite_record_key(&(incoming.thread_id(), incoming.range_start()))?;
                    let mut existing =
                        sqlite_record::<SourceSessionDigestRecord>(connection, &namespace, &key)?
                            .into_iter()
                            .collect::<Vec<_>>();
                    group_digest_records_by_day(source_id, &existing)?;
                    if existing.iter().any(|record| {
                        record.thread_id() != incoming.thread_id()
                            || record.range_start() != incoming.range_start()
                    }) {
                        return Err(invalid_data(
                            "session digest database key does not match its record",
                        ));
                    }
                    let mut index = digest_record_index(&existing)?;
                    if apply_digest_record(&mut existing, &mut index, incoming)? {
                        let record = &existing[0];
                        database::put_record(
                            connection,
                            &namespace,
                            &key,
                            record.retention_through().timestamp_millis(),
                            record,
                        )?;
                        changed = true;
                    }
                }
                if changed {
                    report.shards_written += 1;
                } else {
                    report.shards_skipped += 1;
                }
            }
            // The namespace carries the privacy profile; digests themselves
            // retain source-scoped opaque project identities in both profiles.
            let _ = redaction;
            Ok(report)
        })
    }

    pub(super) fn sqlite_load_digest_records(
        &self,
        source_id: &NodeId,
        since: DateTime<Utc>,
        directory: &Path,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Vec<SourceSessionDigestRecord>> {
        let database = self.sqlite_database().expect("SQLite backend");
        if !database.exists()? {
            return Ok(Vec::new());
        }
        database.read(|connection| {
            let namespace = database.namespace(directory)?;
            let values = database::records::<SourceSessionDigestRecord>(
                connection,
                &namespace,
                since.timestamp_millis(),
                budget,
            )?;
            group_digest_records_by_day(source_id, &values)?;
            let mut records = Vec::new();
            let mut index = HashMap::new();
            for record in values {
                if digest_record_intersects_since(&record, since) {
                    apply_digest_record(&mut records, &mut index, record)?;
                }
            }
            sort_digest_records(&mut records);
            Ok(records)
        })
    }

    pub(super) fn sqlite_read_fact_manifest(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        replica: &SessionReplicaKey,
        shard_key: &ThreadShardKey,
        budget: Option<&mut SourceHistoryReadBudget>,
    ) -> io::Result<Option<ActiveFactManifest>> {
        let database = self.sqlite_database().expect("SQLite backend");
        if !database.exists()? {
            return Ok(None);
        }
        database.read(|connection| {
            let key = manifest_key(self, &database, source_id, redaction, shard_key)?;
            let manifest = sqlite_state_bounded::<ActiveFactManifest>(
                connection,
                &key,
                MAX_FACT_MANIFEST_BYTES,
                budget,
            )?;
            if let Some(manifest) = &manifest {
                validate_active_manifest(
                    manifest,
                    &self.profile_id,
                    source_id,
                    redaction,
                    replica,
                    shard_key,
                )?;
            }
            Ok(manifest)
        })
    }

    pub(super) fn sqlite_read_fact_generation(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        manifest: &ActiveFactManifest,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Vec<UsageEventFactRecord>> {
        let database = self.sqlite_database().expect("SQLite backend");
        database.read(|connection| {
            let namespace = generation_namespace(
                self,
                &database,
                source_id,
                redaction,
                &manifest.thread_shard_key,
                &manifest.active_generation,
            )?;
            let mut records = database::records::<UsageEventFactRecord>(
                connection,
                &namespace,
                i64::MIN,
                budget,
            )?;
            let mut ids = BTreeSet::new();
            for record in &records {
                validate_fact_record_namespace(record, &manifest.replica)?;
                if !ids.insert(record.event_id()) {
                    return Err(invalid_data(
                        "fact generation contains duplicate usage event IDs",
                    ));
                }
                if manifest
                    .retained_since
                    .is_some_and(|cutoff| record.occurred_at() < cutoff)
                {
                    return Err(invalid_data(
                        "active fact generation contains a record below its retention floor",
                    ));
                }
            }
            if records.len() != manifest.record_count
                || record_days(&records) != manifest.shard_days
            {
                return Err(invalid_data(
                    "active fact generation does not match its manifest",
                ));
            }
            sort_fact_records(&mut records);
            validate_fact_generation_limits(&records)?;
            Ok(records)
        })
    }

    pub(super) fn sqlite_load_active_fact_set(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        thread_id: &ThreadId,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Option<ActiveFactSet>> {
        let database = self.sqlite_database().expect("SQLite backend");
        database.read(|_| {
            let source = self.load_source_metadata_with_budget(source_id, budget)?;
            let replica = SessionReplicaKey::new(source_id.clone(), thread_id.clone());
            let shard_key = ThreadShardKey::from_replica(&replica);
            let Some(manifest) = self.sqlite_read_fact_manifest(
                source_id,
                redaction,
                &replica,
                &shard_key,
                Some(budget),
            )?
            else {
                return Ok(None);
            };
            validate_fact_remote_binding(
                source.kind(),
                source_id,
                manifest.remote_binding.as_ref(),
            )?;
            let records =
                self.sqlite_read_fact_generation(source_id, redaction, &manifest, budget)?;
            Ok(Some(ActiveFactSet {
                replica,
                redaction_profile: redaction,
                version: manifest.version(),
                cursor: manifest.cursor,
                remote_binding: manifest.remote_binding.clone(),
                activated_at: manifest.activated_at,
                records,
            }))
        })
    }

    pub(super) fn sqlite_stage_fact_batch(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        batch: &CompleteFactBatch,
    ) -> io::Result<()> {
        batch.validate()?;
        if batch.replica.source_id() != source_id {
            return Err(invalid_data(
                "fact batch source does not match its namespace",
            ));
        }
        let database = self.sqlite_database().expect("SQLite backend");
        let shard_key = ThreadShardKey::from_replica(&batch.replica);
        // Read and prove the candidate outside the writer/config fences. The
        // publication transaction rechecks the complete active version.
        let (source, current, mut records) = database.read(|_| {
            let source = self.load_source_metadata(source_id)?;
            validate_fact_remote_binding(source.kind(), source_id, batch.remote_binding.as_ref())?;
            let current = self.sqlite_read_fact_manifest(
                source_id,
                redaction,
                &batch.replica,
                &shard_key,
                None,
            )?;
            if current.as_ref().map(ActiveFactManifest::version) != batch.expected_active_version {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "active fact version changed before staging",
                ));
            }
            let records = if batch.kind == FactBatchKind::Delta {
                current
                    .as_ref()
                    .map(|manifest| {
                        self.read_fact_generation_unlocked(source_id, redaction, manifest)
                    })
                    .transpose()?
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            Ok((source, current, records))
        })?;
        let retained_since = current
            .as_ref()
            .and_then(|manifest| manifest.retained_since);
        let mut index = records
            .iter()
            .enumerate()
            .map(|(position, record)| (record.event_id.clone(), position))
            .collect::<HashMap<_, _>>();
        for change in &batch.changes {
            validate_fact_record_namespace(change, &batch.replica)?;
            if retained_since.is_some_and(|cutoff| change.occurred_at() < cutoff) {
                continue;
            }
            apply_fact_record(&mut records, &mut index, change.clone())?;
        }
        sort_fact_records(&mut records);
        validate_fact_generation_limits(&records)?;
        validate_proof(
            &batch.replica,
            &records,
            &batch.validated_digests,
            retained_since,
        )?;
        let descriptor = StagedFactBatch {
            format_version: FACT_BATCH_FORMAT_VERSION,
            profile_id: self.profile_id.clone(),
            source_id: source_id.clone(),
            redaction_profile: redaction,
            thread_shard_key: shard_key.clone(),
            batch_id: batch.batch_id.clone(),
            kind: batch.kind,
            replica: batch.replica.clone(),
            expected_active_version: batch.expected_active_version.clone(),
            remote_binding: batch.remote_binding.clone(),
            validated_digests: batch.validated_digests.clone(),
            retained_since,
            activate_cursor: batch.activate_cursor,
            completed_at: batch.completed_at,
            shard_days: record_days(&records),
            change_count: batch.changes.len(),
            record_count: records.len(),
        };
        encode_pretty_bounded(&descriptor, MAX_FACT_MANIFEST_BYTES)?;
        database.write(|connection| {
            let latest_source = self.load_source_metadata(source_id)?;
            if latest_source.kind() != source.kind() {
                return Err(invalid_data("source kind changed before fact staging"));
            }
            let latest = self.sqlite_read_fact_manifest(
                source_id,
                redaction,
                &batch.replica,
                &shard_key,
                None,
            )?;
            if latest.as_ref().map(ActiveFactManifest::version) != batch.expected_active_version {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "active fact version changed before staging",
                ));
            }
            let key = staged_key(
                self,
                &database,
                source_id,
                redaction,
                &batch.batch_id,
                STAGED_BATCH_FILE,
            )?;
            if database::state::<StagedFactBatch>(connection, &key)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "fact batch is already staged",
                ));
            }
            let namespace = generation_namespace(
                self,
                &database,
                source_id,
                redaction,
                &shard_key,
                &batch.batch_id,
            )?;
            let occupied: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM history_records WHERE namespace=?1 LIMIT 1)",
                    [&namespace],
                    |row| row.get(0),
                )
                .map_err(database::sql_error)?;
            if occupied
                || latest
                    .as_ref()
                    .is_some_and(|manifest| manifest.active_generation == batch.batch_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "fact generation already exists for staged batch",
                ));
            }
            for record in &records {
                database::put_record(
                    connection,
                    &namespace,
                    record.event_id().as_str(),
                    record.occurred_at().timestamp_millis(),
                    record,
                )?;
            }
            database::set_state(connection, &key, &descriptor)?;
            database::set_state(
                connection,
                &staged_key(
                    self,
                    &database,
                    source_id,
                    redaction,
                    &batch.batch_id,
                    "staged-at.json",
                )?,
                &Utc::now(),
            )?;
            ensure_sql_fact_cap(self, &database, connection, source_id, redaction)
        })
    }

    pub(super) fn sqlite_prevalidate_fact_batch(
        &self,
        source_id: &NodeId,
        redaction: RedactionProfile,
        batch_id: &FactBatchId,
    ) -> io::Result<PrevalidatedFactPublication> {
        let database = self.sqlite_database().expect("SQLite backend");
        let (descriptor, current, manifest) = database.read(|connection| {
            let source = self.load_source_metadata(source_id)?;
            let descriptor =
                read_descriptor(self, &database, connection, source_id, redaction, batch_id)?;
            validate_fact_remote_binding(
                source.kind(),
                source_id,
                descriptor.remote_binding.as_ref(),
            )?;
            let current = self.sqlite_read_fact_manifest(
                source_id,
                redaction,
                &descriptor.replica,
                &descriptor.thread_shard_key,
                None,
            )?;
            let already_active = current.as_ref().is_some_and(|manifest| {
                manifest.active_generation == descriptor.batch_id
                    && manifest.cursor == descriptor.activate_cursor
                    && manifest.remote_binding == descriptor.remote_binding
            });
            if already_active {
                return Ok((descriptor, current, None));
            }
            if current.as_ref().map(ActiveFactManifest::version)
                != descriptor.expected_active_version
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "active fact version changed before activation",
                ));
            }
            let empty_delta = descriptor.kind == FactBatchKind::Delta
                && descriptor
                    .expected_active_version
                    .as_ref()
                    .is_some_and(|expected| {
                        expected.cursor == descriptor.activate_cursor
                            && expected.validated_digests == descriptor.validated_digests
                    });
            if empty_delta {
                return Ok((descriptor, current, None));
            }
            let manifest = ActiveFactManifest {
                format_version: FACT_MANIFEST_FORMAT_VERSION,
                profile_id: self.profile_id.clone(),
                source_id: source_id.clone(),
                redaction_profile: redaction,
                thread_shard_key: descriptor.thread_shard_key.clone(),
                replica: descriptor.replica.clone(),
                active_generation: descriptor.batch_id.clone(),
                cursor: descriptor.activate_cursor,
                remote_binding: descriptor.remote_binding.clone(),
                validated_digests: descriptor.validated_digests.clone(),
                retained_since: descriptor.retained_since,
                activated_at: descriptor.completed_at,
                shard_days: descriptor.shard_days.clone(),
                record_count: descriptor.record_count,
            };
            let mut budget = SourceHistoryReadBudget::for_query();
            let records =
                self.sqlite_read_fact_generation(source_id, redaction, &manifest, &mut budget)?;
            validate_proof(
                &descriptor.replica,
                &records,
                &descriptor.validated_digests,
                descriptor.retained_since,
            )?;
            Ok((descriptor, current, Some(manifest)))
        })?;
        let Some(manifest) = manifest else {
            return Ok(PrevalidatedFactPublication {
                descriptor,
                mode: PrevalidatedFactPublicationMode::NoOp,
            });
        };
        encode_pretty_bounded(&manifest, MAX_FACT_MANIFEST_BYTES)?;
        database.write(|connection| {
            let persisted =
                read_descriptor(self, &database, connection, source_id, redaction, batch_id)?;
            if persisted != descriptor {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "staged fact descriptor changed after prevalidation",
                ));
            }
            database::set_state(
                connection,
                &staged_key(
                    self,
                    &database,
                    source_id,
                    redaction,
                    batch_id,
                    STAGED_PUBLICATION_FILE,
                )?,
                &manifest,
            )?;
            ensure_sql_fact_cap(self, &database, connection, source_id, redaction)
        })?;
        Ok(PrevalidatedFactPublication {
            descriptor,
            mode: PrevalidatedFactPublicationMode::Publish {
                manifest: Box::new(manifest),
                previous_active_generation: current.map(|manifest| manifest.active_generation),
            },
        })
    }

    pub(super) fn sqlite_publish_fact_batch(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> io::Result<FactActivationReport> {
        let database = self.sqlite_database().expect("SQLite backend");
        let descriptor = &publication.descriptor;
        database.write_nowait(|connection| {
            let source = self.load_source_metadata(&descriptor.source_id)?;
            validate_fact_remote_binding(
                source.kind(),
                &descriptor.source_id,
                descriptor.remote_binding.as_ref(),
            )?;
            if read_descriptor(
                self,
                &database,
                connection,
                &descriptor.source_id,
                descriptor.redaction_profile,
                &descriptor.batch_id,
            )? != *descriptor
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "staged fact descriptor changed after prevalidation",
                ));
            }
            let current = self.sqlite_read_fact_manifest(
                &descriptor.source_id,
                descriptor.redaction_profile,
                &descriptor.replica,
                &descriptor.thread_shard_key,
                None,
            )?;
            match &publication.mode {
                PrevalidatedFactPublicationMode::NoOp => {
                    let already_active = current.as_ref().is_some_and(|manifest| {
                        manifest.active_generation == descriptor.batch_id
                            && manifest.cursor == descriptor.activate_cursor
                            && manifest.remote_binding == descriptor.remote_binding
                    });
                    let empty_delta = descriptor.kind == FactBatchKind::Delta
                        && current.as_ref().map(ActiveFactManifest::version)
                            == descriptor.expected_active_version
                        && descriptor
                            .expected_active_version
                            .as_ref()
                            .is_some_and(|expected| {
                                expected.cursor == descriptor.activate_cursor
                                    && expected.validated_digests == descriptor.validated_digests
                            });
                    if !already_active && !empty_delta {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "active fact version changed before no-op publication",
                        ));
                    }
                    Ok(FactActivationReport {
                        activated: false,
                        cleanup_pending: false,
                    })
                }
                PrevalidatedFactPublicationMode::Publish { manifest, .. } => {
                    if current.as_ref().map(ActiveFactManifest::version)
                        != descriptor.expected_active_version
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "active fact version changed before publication",
                        ));
                    }
                    let key = staged_key(
                        self,
                        &database,
                        &descriptor.source_id,
                        descriptor.redaction_profile,
                        &descriptor.batch_id,
                        STAGED_PUBLICATION_FILE,
                    )?;
                    let candidate = sqlite_state_bounded::<ActiveFactManifest>(
                        connection,
                        &key,
                        MAX_FACT_MANIFEST_BYTES,
                        None,
                    )?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "prevalidated fact manifest is missing",
                        )
                    })?;
                    validate_active_manifest(
                        &candidate,
                        &self.profile_id,
                        &descriptor.source_id,
                        descriptor.redaction_profile,
                        &descriptor.replica,
                        &descriptor.thread_shard_key,
                    )?;
                    if &candidate != manifest.as_ref() {
                        return Err(invalid_data(
                            "prevalidated fact manifest changed before publication",
                        ));
                    }
                    // Generation rows and candidate proof were committed together
                    // before entering the external config fence; GC cannot remove
                    // a nonexpired staged generation inside this transaction.
                    database::set_state(
                        connection,
                        &manifest_key(
                            self,
                            &database,
                            &descriptor.source_id,
                            descriptor.redaction_profile,
                            &descriptor.thread_shard_key,
                        )?,
                        &candidate,
                    )?;
                    database::delete_state(connection, &key)?;
                    Ok(FactActivationReport {
                        activated: true,
                        cleanup_pending: false,
                    })
                }
            }
        })
    }

    pub(super) fn sqlite_cleanup_fact_publication(
        &self,
        publication: &PrevalidatedFactPublication,
    ) -> io::Result<()> {
        let database = self.sqlite_database().expect("SQLite backend");
        let descriptor = &publication.descriptor;
        database.write(|connection| {
            let current = self.sqlite_read_fact_manifest(
                &descriptor.source_id,
                descriptor.redaction_profile,
                &descriptor.replica,
                &descriptor.thread_shard_key,
                None,
            )?;
            let ours_active = current
                .as_ref()
                .is_some_and(|manifest| manifest.active_generation == descriptor.batch_id);
            if let PrevalidatedFactPublicationMode::Publish {
                previous_active_generation: Some(previous),
                ..
            } = &publication.mode
                && ours_active
                && previous != &descriptor.batch_id
            {
                database::delete_namespace(
                    connection,
                    &generation_namespace(
                        self,
                        &database,
                        &descriptor.source_id,
                        descriptor.redaction_profile,
                        &descriptor.thread_shard_key,
                        previous,
                    )?,
                )?;
            }
            if !ours_active {
                database::delete_namespace(
                    connection,
                    &generation_namespace(
                        self,
                        &database,
                        &descriptor.source_id,
                        descriptor.redaction_profile,
                        &descriptor.thread_shard_key,
                        &descriptor.batch_id,
                    )?,
                )?;
            }
            for file in [STAGED_BATCH_FILE, STAGED_PUBLICATION_FILE, "staged-at.json"] {
                database::delete_state(
                    connection,
                    &staged_key(
                        self,
                        &database,
                        &descriptor.source_id,
                        descriptor.redaction_profile,
                        &descriptor.batch_id,
                        file,
                    )?,
                )?;
            }
            ensure_sql_fact_cap(
                self,
                &database,
                connection,
                &descriptor.source_id,
                descriptor.redaction_profile,
            )
        })
    }
}

fn active_manifests(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<Vec<(String, ActiveFactManifest)>> {
    let prefix = format!(
        "{}/",
        database.namespace(&store.source_fact_manifests_directory(source_id, redaction))?
    );
    let mut manifests = Vec::new();
    for key in database::state_keys(connection, &prefix)? {
        let Some(name) = key
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".json"))
        else {
            return Err(invalid_data("invalid SQL fact manifest key"));
        };
        let shard_key = name
            .parse::<ThreadShardKey>()
            .map_err(|error| invalid_data(error.to_string()))?;
        let manifest = sqlite_state_bounded::<ActiveFactManifest>(
            connection,
            &key,
            MAX_FACT_MANIFEST_BYTES,
            None,
        )?
        .ok_or_else(|| invalid_data("fact manifest disappeared inside SQLite snapshot"))?;
        validate_active_manifest(
            &manifest,
            &store.profile_id,
            source_id,
            redaction,
            &manifest.replica,
            &shard_key,
        )?;
        manifests.push((key, manifest));
    }
    Ok(manifests)
}

pub(super) fn earliest_session_evidence_time(
    store: &SourceHistoryStore,
    sources: &[SourceMetadata],
    redactions: &[RedactionProfile],
) -> io::Result<Option<DateTime<Utc>>> {
    let database = store.sqlite_database().expect("SQLite backend");
    if !database.exists()? {
        return Ok(None);
    }
    database.read(|connection| {
        let mut earliest = None;
        for source in sources {
            for &redaction in redactions {
                let prefix = format!("{}/", database.namespace(&store.source_directory(source.source_id()).join(redaction.directory_name()))?);
                let mut statement = connection.prepare("SELECT DISTINCT namespace FROM history_records WHERE substr(namespace,1,length(?1))=?1 AND substr(namespace,-8)='/digests'").map_err(database::sql_error)?;
                let namespaces = statement.query_map([&prefix], |row| row.get::<_, String>(0)).map_err(database::sql_error)?.collect::<Result<Vec<_>, _>>().map_err(database::sql_error)?;
                for namespace in namespaces {
                    let mut budget = SourceHistoryReadBudget::for_query();
                    for record in database::records::<SourceSessionDigestRecord>(connection, &namespace, i64::MIN, &mut budget)? {
                        record.validate()?;
                        let timestamp = record.range_start().date_naive().and_hms_opt(0, 0, 0).expect("midnight").and_utc();
                        earliest = Some(earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)));
                    }
                }
                for (_, manifest) in active_manifests(store, &database, connection, source.source_id(), redaction)? {
                    if let Some(day) = manifest.shard_days.first() {
                        let timestamp = day.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
                        earliest = Some(earliest.map_or(timestamp, |current: DateTime<Utc>| current.min(timestamp)));
                    }
                }
            }
        }
        Ok(earliest)
    })
}

pub(super) fn garbage_collect_session_evidence(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction: RedactionProfile,
    cutoff_day: NaiveDate,
    trusted_at: DateTime<Utc>,
) -> io::Result<usize> {
    let database = store.sqlite_database().expect("SQLite backend");
    database.write(|connection| {
        let cutoff = cutoff_day.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
        let prefix = format!("{}/", database.namespace(&store.source_directory(source_id).join(redaction.directory_name()))?);
        let mut statement = connection.prepare("SELECT DISTINCT namespace FROM history_records WHERE substr(namespace,1,length(?1))=?1 AND substr(namespace,-8)='/digests'").map_err(database::sql_error)?;
        let namespaces = statement.query_map([&prefix], |row| row.get::<_, String>(0)).map_err(database::sql_error)?.collect::<Result<Vec<_>, _>>().map_err(database::sql_error)?;
        let mut pruned = 0;
        for namespace in namespaces {
            let mut budget = SourceHistoryReadBudget::for_query();
            let records = database::records::<SourceSessionDigestRecord>(connection, &namespace, i64::MIN, &mut budget)?;
            group_digest_records_by_day(source_id, &records)?;
            let mut old_days = BTreeSet::new();
            let mut retained_days = BTreeSet::new();
            for record in records {
                if record.range_start().date_naive() < cutoff_day && record.retention_through() < cutoff {
                    database::delete_record(connection, &namespace, &sqlite_record_key(&(record.thread_id(), record.range_start()))?)?;
                    old_days.insert(record.range_start().date_naive());
                } else { retained_days.insert(record.range_start().date_naive()); }
            }
            pruned += old_days.difference(&retained_days).count();
        }
        for (key, manifest) in active_manifests(store, &database, connection, source_id, redaction)? {
            let retained_since = manifest.retained_since.map_or(cutoff, |current| current.max(cutoff));
            if manifest.retained_since == Some(retained_since) { continue; }
            let mut budget = SourceHistoryReadBudget::for_query();
            let records = store.sqlite_read_fact_generation(source_id, redaction, &manifest, &mut budget)?;
            let retained = records.iter().filter(|record| record.occurred_at() >= retained_since).cloned().collect::<Vec<_>>();
            if retained.len() == records.len() {
                database::set_state(connection, &key, &ActiveFactManifest { retained_since: Some(retained_since), ..manifest })?;
                continue;
            }
            let replacement = FactBatchId::generate()?;
            let namespace = generation_namespace(store, &database, source_id, redaction, &manifest.thread_shard_key, &replacement)?;
            for record in &retained { database::put_record(connection, &namespace, record.event_id().as_str(), record.occurred_at().timestamp_millis(), record)?; }
            let replacement_manifest = ActiveFactManifest { active_generation: replacement, retained_since: Some(retained_since), shard_days: record_days(&retained), record_count: retained.len(), ..manifest.clone() };
            database::set_state(connection, &key, &replacement_manifest)?;
            database::delete_namespace(connection, &generation_namespace(store, &database, source_id, redaction, &manifest.thread_shard_key, &manifest.active_generation)?)?;
            pruned += manifest.shard_days.iter().filter(|day| **day < cutoff_day).count();
        }
        garbage_collect_artifacts(store, &database, connection, source_id, redaction, trusted_at)?;
        ensure_sql_fact_cap(store, &database, connection, source_id, redaction)?;
        Ok(pruned)
    })
}

fn garbage_collect_artifacts(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
    trusted_at: DateTime<Utc>,
) -> io::Result<()> {
    let expires_before = trusted_at
        .checked_sub_signed(Duration::hours(FACT_STAGING_TTL_HOURS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let prefix = format!(
        "{}/",
        database.namespace(&store.source_fact_staging_directory(source_id, redaction))?
    );
    let mut referenced = BTreeSet::new();
    for key in database::state_keys(connection, &prefix)? {
        let Some(batch_name) = key
            .strip_prefix(&prefix)
            .and_then(|relative| relative.strip_suffix("/batch.json"))
        else {
            continue;
        };
        let batch_id = batch_name
            .parse::<FactBatchId>()
            .map_err(|error| invalid_data(error.to_string()))?;
        let descriptor =
            read_descriptor(store, database, connection, source_id, redaction, &batch_id)?;
        let staged_at = database::state::<DateTime<Utc>>(
            connection,
            &staged_key(
                store,
                database,
                source_id,
                redaction,
                &batch_id,
                "staged-at.json",
            )?,
        )?
        .ok_or_else(|| invalid_data("staged fact batch has no central staging timestamp"))?;
        let namespace = generation_namespace(
            store,
            database,
            source_id,
            redaction,
            &descriptor.thread_shard_key,
            &batch_id,
        )?;
        if staged_at < expires_before {
            for file in [STAGED_BATCH_FILE, STAGED_PUBLICATION_FILE, "staged-at.json"] {
                database::delete_state(
                    connection,
                    &staged_key(store, database, source_id, redaction, &batch_id, file)?,
                )?;
            }
        } else {
            referenced.insert(namespace);
        }
    }
    for (_, manifest) in active_manifests(store, database, connection, source_id, redaction)? {
        referenced.insert(generation_namespace(
            store,
            database,
            source_id,
            redaction,
            &manifest.thread_shard_key,
            &manifest.active_generation,
        )?);
    }
    let facts_prefix = format!(
        "{}/",
        database.namespace(&store.source_facts_directory(source_id, redaction))?
    );
    let mut statement = connection.prepare("SELECT DISTINCT namespace FROM history_records WHERE substr(namespace,1,length(?1))=?1").map_err(database::sql_error)?;
    let namespaces = statement
        .query_map([&facts_prefix], |row| row.get::<_, String>(0))
        .map_err(database::sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(database::sql_error)?;
    for namespace in namespaces {
        if !referenced.contains(&namespace) {
            database::delete_namespace(connection, &namespace)?;
        }
    }
    Ok(())
}

fn import_generation(
    connection: &rusqlite::Connection,
    namespace: &str,
    records: &[UsageEventFactRecord],
) -> io::Result<()> {
    let mut budget = SourceHistoryReadBudget::for_query();
    let mut existing =
        database::records::<UsageEventFactRecord>(connection, namespace, i64::MIN, &mut budget)?;
    if !existing.is_empty() {
        sort_fact_records(&mut existing);
        if existing != records {
            return Err(invalid_data(
                "legacy fact generation conflicts with existing SQLite records",
            ));
        }
        return Ok(());
    }
    for record in records {
        database::put_record(
            connection,
            namespace,
            record.event_id().as_str(),
            record.occurred_at().timestamp_millis(),
            record,
        )?;
    }
    Ok(())
}

pub(super) fn import_legacy_evidence(
    target: &SourceHistoryStore,
    legacy: &SourceHistoryStore,
    sources: &[SourceMetadata],
) -> io::Result<()> {
    let database = target.sqlite_database().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "evidence import target must use SQLite",
        )
    })?;
    database.write(|connection| {
        let prior_migration = !database::state_keys(connection, "database/migration/")?.is_empty();
        let cutoff = if prior_migration { target.sqlite_retention_cutoff(connection)? } else { None };
        for source in sources {
            // Re-pairing a previously purged node must not restore facts or
            // digests from its immutable legacy backup after the first import.
            if prior_migration && source.kind() == SourceKind::Ssh {
                continue;
            }
            for redaction in [RedactionProfile::PreviewEnabled, RedactionProfile::Redacted] {
                let directory = legacy.source_digests_directory(source.source_id(), redaction);
                let mut budget = SourceHistoryReadBudget::for_query();
                let mut digests = legacy.load_source_session_digest_records_from_directory_with_budget(source.source_id(), redaction, DateTime::<Utc>::MIN_UTC, &directory, &mut budget)?;
                digests.retain(|record| cutoff.is_none_or(|cutoff| record.retention_through() >= cutoff));
                target.sqlite_record_digest_changes(source.source_id(), redaction, &target.source_digests_directory(source.source_id(), redaction), &digests)?;
                let manifests = legacy.source_fact_manifests_directory(source.source_id(), redaction);
                if legacy.private_directory_exists(&manifests)? {
                    for (shard_key, path) in fact_manifest_entries(legacy, &manifests)? {
                        let manifest: ActiveFactManifest = read_json_file(&path, MAX_FACT_MANIFEST_BYTES)?;
                        validate_active_manifest(&manifest, &legacy.profile_id, source.source_id(), redaction, &manifest.replica, &shard_key)?;
                        validate_fact_remote_binding(source.kind(), source.source_id(), manifest.remote_binding.as_ref())?;
                        let records = legacy.read_fact_generation_unlocked(source.source_id(), redaction, &manifest)?;
                        validate_proof(&manifest.replica, &records, &manifest.validated_digests, manifest.retained_since)?;
                        // A later redaction migration must never replace an
                        // already committed SQL cursor/proof with its old backup.
                        if let Some(existing) = target.sqlite_read_fact_manifest(source.source_id(), redaction, &manifest.replica, &shard_key, None)? {
                            let mut budget = SourceHistoryReadBudget::for_query();
                            let existing_records = target.sqlite_read_fact_generation(source.source_id(), redaction, &existing, &mut budget)?;
                            validate_proof(&existing.replica, &existing_records, &existing.validated_digests, existing.retained_since)?;
                            continue;
                        }
                        let namespace = generation_namespace(target, &database, source.source_id(), redaction, &shard_key, &manifest.active_generation)?;
                        import_generation(connection, &namespace, &records)?;
                        database::set_state(connection, &manifest_key(target, &database, source.source_id(), redaction, &shard_key)?, &manifest)?;
                    }
                }
                let staging_root = legacy.source_fact_staging_directory(source.source_id(), redaction);
                if legacy.private_directory_exists(&staging_root)? {
                    for entry in fs::read_dir(&staging_root)? {
                        let entry = entry?;
                        if entry.file_name() == OsStr::new(FACT_STAGING_LOCK_FILE) { continue; }
                        if is_atomic_fact_manifest_temporary_file(&entry.file_name()) { continue; }
                        let name = entry.file_name();
                        let batch_id = name.to_str().ok_or_else(|| invalid_data("fact staging name is not UTF-8"))?.parse::<FactBatchId>().map_err(|error| invalid_data(error.to_string()))?;
                        let staging = entry.path();
                        legacy.validate_private_path(&staging)?;
                        let descriptor = match read_staged_batch(&staging.join(STAGED_BATCH_FILE), &legacy.profile_id, source.source_id(), redaction, &batch_id) {
                            Ok(descriptor) => descriptor,
                            // A interrupted creation without a descriptor never
                            // became a complete durable batch.
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error),
                        };
                        validate_fact_remote_binding(source.kind(), source.source_id(), descriptor.remote_binding.as_ref())?;
                        let staged_generation = staging.join(STAGED_GENERATION_DIRECTORY);
                        let generation = if legacy.private_directory_exists(&staged_generation)? { staged_generation } else { legacy.source_facts_directory(source.source_id(), redaction).join(descriptor.thread_shard_key.as_str()).join(batch_id.as_str()) };
                        let records = read_fact_generation(legacy, &generation, &legacy.profile_id, source.source_id(), redaction, &descriptor.replica, &descriptor.thread_shard_key, &batch_id, &descriptor.shard_days)?;
                        if records.len() != descriptor.record_count { return Err(invalid_data("legacy staged fact record count does not match descriptor")); }
                        validate_proof(&descriptor.replica, &records, &descriptor.validated_digests, descriptor.retained_since)?;
                        let descriptor_key = staged_key(target, &database, source.source_id(), redaction, &batch_id, STAGED_BATCH_FILE)?;
                        if let Some(existing) = sqlite_state_bounded::<StagedFactBatch>(connection, &descriptor_key, MAX_FACT_MANIFEST_BYTES, None)? {
                            if existing != descriptor { return Err(invalid_data("legacy staging conflicts with an existing SQLite fact descriptor")); }
                            continue;
                        }
                        let namespace = generation_namespace(target, &database, source.source_id(), redaction, &descriptor.thread_shard_key, &batch_id)?;
                        import_generation(connection, &namespace, &records)?;
                        database::set_state(connection, &descriptor_key, &descriptor)?;
                        let staged_at = DateTime::<Utc>::from(fs::symlink_metadata(&staging)?.modified()?);
                        database::set_state(connection, &staged_key(target, &database, source.source_id(), redaction, &batch_id, "staged-at.json")?, &staged_at)?;
                        if let Some(candidate) = read_optional_json_file::<ActiveFactManifest>(&staging.join(STAGED_PUBLICATION_FILE), MAX_FACT_MANIFEST_BYTES)? {
                            validate_active_manifest(&candidate, &legacy.profile_id, source.source_id(), redaction, &descriptor.replica, &descriptor.thread_shard_key)?;
                            if candidate.active_generation != batch_id || candidate.cursor != descriptor.activate_cursor || candidate.validated_digests != descriptor.validated_digests || candidate.remote_binding != descriptor.remote_binding || candidate.retained_since != descriptor.retained_since || candidate.record_count != descriptor.record_count || candidate.shard_days != descriptor.shard_days { return Err(invalid_data("legacy candidate fact publication differs from its descriptor")); }
                            database::set_state(connection, &staged_key(target, &database, source.source_id(), redaction, &batch_id, STAGED_PUBLICATION_FILE)?, &candidate)?;
                        }
                    }
                }
                ensure_sql_fact_cap(target, &database, connection, source.source_id(), redaction)?;
            }
        }
        Ok(())
    })
}
