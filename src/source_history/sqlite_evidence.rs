//! Typed session evidence and immutable fact publication in SQLite.
//!
//! Replica namespaces remain source- and thread-specific SQL keys. A staged generation
//! is invisible until the active manifest, digest proof and cursor are published
//! together by a short compare-and-swap transaction.
use super::*;
#[path = "sqlite_evidence_gc.rs"]
mod gc;
pub(crate) use gc::prepare_gc_evidence_unit;

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
    ensure_sql_fact_cap_with_limit(
        store,
        database,
        connection,
        source_id,
        redaction,
        MAX_FACT_NAMESPACE_BYTES,
    )
}

fn ensure_sql_fact_cap_with_limit(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
    maximum_bytes: u64,
) -> io::Result<()> {
    let root = format!(
        "{}/",
        database.namespace(
            &store
                .source_directory(source_id)
                .join(redaction.directory_name())
        )?
    );
    // Bound generation namespaces and state descriptors as well as bytes;
    // per-generation event limits are enforced separately.
    let gc_exempt = gc::exempt_namespace(store, database, connection, source_id, redaction)?;
    let (record_bytes, generation_count): (i64, i64) = connection.query_row(
        "SELECT COALESCE(sum(length(payload)),0),count(DISTINCT namespace) FROM history_records WHERE substr(namespace,1,length(?1))=?1 AND substr(namespace,length(?1)+1,6)='facts/' AND namespace<>?2", rusqlite::params![&root, gc_exempt.as_deref().unwrap_or("")], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(database::sql_error)?;
    let (state_bytes, state_count): (i64, i64) = connection.query_row(
        "SELECT COALESCE(sum(length(payload)),0),count(*) FROM history_state WHERE substr(state_key,1,length(?1))=?1 AND (substr(state_key,length(?1)+1,15)='fact-manifests/' OR substr(state_key,length(?1)+1,13)='fact-staging/')", [&root], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(database::sql_error)?;
    let allowance = gc::bookkeeping_allowance(store, database, connection, source_id, redaction)?;
    let usage = FactNamespaceUsage {
        bytes: u64::try_from(
            record_bytes
                .checked_add(state_bytes)
                .ok_or_else(|| invalid_data("fact namespace size overflowed"))?,
        )
        .map_err(|_| invalid_data("invalid SQL fact namespace size"))?
        .checked_sub(allowance)
        .ok_or_else(|| invalid_data("GC bookkeeping allowance exceeds stored facts metadata"))?,
        entries: u64::try_from(
            generation_count
                .checked_add(state_count)
                .ok_or_else(|| invalid_data("fact namespace entry count overflowed"))?,
        )
        .map_err(|_| invalid_data("invalid SQL fact namespace entry count"))?,
    };
    validate_fact_namespace_usage(usage, maximum_bytes, MAX_FACT_NAMESPACE_ENTRIES)
}

#[cfg(test)]
pub(super) fn ensure_sql_fact_cap_for_test(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source_id: &NodeId,
    redaction: RedactionProfile,
    maximum_bytes: u64,
) -> io::Result<()> {
    ensure_sql_fact_cap_with_limit(
        store,
        database,
        connection,
        source_id,
        redaction,
        maximum_bytes,
    )
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
                        let mut budget = SourceHistoryReadBudget::for_query();
                        self.sqlite_read_fact_generation(
                            source_id,
                            redaction,
                            manifest,
                            &mut budget,
                        )
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
                    self.advance_facts_projection_revision(connection)?;
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

#[cfg(test)]
pub(super) fn garbage_collect_session_evidence(
    store: &SourceHistoryStore,
    source_id: &NodeId,
    redaction: RedactionProfile,
    cutoff_day: NaiveDate,
    trusted_at: DateTime<Utc>,
) -> io::Result<(usize, bool)> {
    let database = store.sqlite_database().expect("SQLite backend");
    let cutoff = cutoff_day.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    let mut pruned = 0;
    let mut changed = false;
    // Legacy small-fixture helpers drive the same bounded production units.
    // Production performs one unit per poll and releases its ownership lease.
    for _ in 0..10_000 {
        let mut budget =
            crate::source_history::sqlite_gc::Budget::with_limits(4096, MAX_SHARD_FILE_BYTES);
        let prepared =
            prepare_gc_evidence_unit(store, source_id, redaction, cutoff, trusted_at, &mut budget)?;
        let report = database.write(|connection| prepared.publish(store, &database, connection))?;
        pruned += report.pruned;
        changed |= report.visible_changed;
        if report.complete {
            return Ok((pruned, changed));
        }
    }
    Err(io::Error::other(
        "test evidence GC did not finish within its bounded unit limit",
    ))
}
