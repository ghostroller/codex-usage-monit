//! Candidate generations are copied a page at a time and activated by exact CAS.
use super::*;
use crate::source_history::sqlite_gc::{self, Budget, RawRow, RowCursor};

const PROGRESS_FILE: &str = "fact-retention-progress.json";
const MAX_PROGRESS_BYTES: u64 = MAX_FACT_MANIFEST_BYTES * 2;
const MAX_DAY_STATISTICS: usize = 4096;
const BOOKKEEPING_FILE: &str = "fact-retention-bookkeeping.json";
const MAX_BOOKKEEPING_PER_MANIFEST: u64 = 64;
const MAX_BOOKKEEPING_BYTES: u64 = MAX_BOOKKEEPING_PER_MANIFEST * MAX_FACT_NAMESPACE_ENTRIES;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
enum Phase {
    Digests,
    Facts,
    Staging,
    Orphans,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Progress {
    cutoff: DateTime<Utc>,
    trusted_at: DateTime<Utc>,
    phase: Phase,
    cursor: Option<RowCursor>,
    state_cursor: String,
    job: Option<FactJob>,
    expired_days: BTreeSet<NaiveDate>,
    retained_days: BTreeSet<NaiveDate>,
    digest_namespace: Option<String>,
    digest_statistics_truncated: bool,
    statistics_partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FactJob {
    key: String,
    expected: ActiveFactManifest,
    replacement: FactBatchId,
    cursor: Option<RowCursor>,
    seen_count: usize,
    seen_days: BTreeSet<NaiveDate>,
    retained_count: usize,
    retained_days: BTreeSet<NaiveDate>,
    retained_bytes: u64,
    retire_namespace: Option<String>,
    cancelled: bool,
}

#[derive(Debug)]
enum Action {
    None,
    Delete(Vec<RawRow>),
    Copy {
        namespace: String,
        rows: Vec<RawRow>,
    },
    Publish {
        key: String,
        expected: Box<ActiveFactManifest>,
        replacement: Box<ActiveFactManifest>,
    },
    DeleteStates(Vec<(String, Vec<u8>)>),
}

pub(crate) struct PreparedEvidenceGc {
    key: String,
    expected: Option<Progress>,
    next: Progress,
    action: Action,
    digest_pruned: usize,
    references: Vec<(String, Option<Vec<u8>>)>,
    source_metadata: (String, Vec<u8>),
    source: NodeId,
    redaction: RedactionProfile,
}

pub(crate) struct EvidenceGcReport {
    pub complete: bool,
    pub visible_changed: bool,
    pub pruned: usize,
    pub statistics_partial: bool,
}

fn progress_key(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<String> {
    database.namespace(
        &store
            .source_directory(source)
            .join(redaction.directory_name())
            .join(PROGRESS_FILE),
    )
}

fn bookkeeping_key(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<String> {
    database.namespace(
        &store
            .source_directory(source)
            .join(redaction.directory_name())
            .join(BOOKKEEPING_FILE),
    )
}

pub(super) fn bookkeeping_allowance(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<u64> {
    let allowance = sqlite_state_bounded::<u64>(
        connection,
        &bookkeeping_key(store, database, source, redaction)?,
        32,
        None,
    )?
    .unwrap_or(0);
    if allowance > MAX_BOOKKEEPING_BYTES {
        return Err(invalid_data(
            "GC bookkeeping allowance exceeds its independent hard cap",
        ));
    }
    Ok(allowance)
}

fn raw_state(
    connection: &rusqlite::Connection,
    key: &str,
    budget: &mut Budget,
) -> io::Result<Option<Vec<u8>>> {
    let mut statement = connection
        .prepare("SELECT payload FROM history_state WHERE state_key=?1")
        .map_err(database::sql_error)?;
    let mut rows = statement.query([key]).map_err(database::sql_error)?;
    let Some(row) = rows.next().map_err(database::sql_error)? else {
        return Ok(None);
    };
    let payload = row
        .get_ref(0)
        .map_err(database::sql_error)?
        .as_blob()
        .map_err(|e| invalid_data(e.to_string()))?;
    if payload.len() as u64 > MAX_PROGRESS_BYTES {
        return Err(invalid_data("GC state exceeds its size budget"));
    }
    budget.charge(payload.len())?;
    Ok(Some(payload.to_vec()))
}

fn candidate_namespace(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    source: &NodeId,
    redaction: RedactionProfile,
    job: &FactJob,
) -> io::Result<String> {
    generation_namespace(
        store,
        database,
        source,
        redaction,
        &job.expected.thread_shard_key,
        &job.replacement,
    )
}

fn validate_job(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    job: &FactJob,
) -> io::Result<()> {
    validate_active_manifest(
        &job.expected,
        &store.profile_id,
        source,
        redaction,
        &job.expected.replica,
        &job.expected.thread_shard_key,
    )?;
    if job.replacement == job.expected.active_generation
        || job.seen_count > job.expected.record_count
        || job.retained_count > job.seen_count
        || job.retained_bytes > MAX_FACT_NAMESPACE_BYTES
        || !job.retained_days.is_subset(&job.seen_days)
    {
        return Err(invalid_data(
            "GC candidate progress counters or generation are invalid",
        ));
    }
    validate_sorted_unique_days(&job.seen_days.iter().copied().collect::<Vec<_>>())?;
    let database = store.sqlite_database().expect("SQLite backend");
    if job.key
        != super::manifest_key(
            store,
            &database,
            source,
            redaction,
            &job.expected.thread_shard_key,
        )?
    {
        return Err(invalid_data(
            "GC candidate manifest key does not match its namespace",
        ));
    }
    let original = generation_namespace(
        store,
        &database,
        source,
        redaction,
        &job.expected.thread_shard_key,
        &job.expected.active_generation,
    )?;
    if job.retire_namespace.is_none()
        && job
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.namespace != original)
    {
        return Err(invalid_data(
            "GC fact cursor does not match its immutable generation",
        ));
    }
    Ok(())
}

/// Exactly one managed candidate/retired generation may temporarily coexist
/// with active facts. It has its own 512 MiB ceiling and is never a query input.
pub(super) fn exempt_namespace(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<Option<String>> {
    let key = progress_key(store, database, source, redaction)?;
    let Some(progress) =
        sqlite_state_bounded::<Progress>(connection, &key, MAX_PROGRESS_BYTES, None)?
    else {
        return Ok(None);
    };
    let Some(job) = progress.job else {
        return Ok(None);
    };
    validate_job(store, source, redaction, &job)?;
    let candidate = candidate_namespace(store, database, source, redaction, &job)?;
    if let Some(namespace) = &job.retire_namespace {
        let original = generation_namespace(
            store,
            database,
            source,
            redaction,
            &job.expected.thread_shard_key,
            &job.expected.active_generation,
        )?;
        if namespace != &candidate && namespace != &original {
            return Err(invalid_data("GC retirement namespace is invalid"));
        }
        Ok(Some(namespace.clone()))
    } else {
        Ok(Some(candidate))
    }
}

pub(crate) fn prepare_gc_evidence_unit(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    cutoff: DateTime<Utc>,
    trusted_at: DateTime<Utc>,
    budget: &mut Budget,
) -> io::Result<PreparedEvidenceGc> {
    let database = store.sqlite_database().expect("SQLite backend");
    let key = progress_key(store, &database, source, redaction)?;
    let metadata_key =
        database.namespace(&store.source_directory(source).join(SOURCE_METADATA_FILE))?;
    let metadata_bytes = database
        .read(|connection| raw_state(connection, &metadata_key, budget))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "GC source was removed during preparation",
            )
        })?;
    if metadata_bytes.len() as u64 > MAX_METADATA_FILE_BYTES {
        return Err(invalid_data("GC source metadata exceeds its size limit"));
    }
    let envelope: SourceMetadataEnvelope =
        serde_json::from_slice(&metadata_bytes).map_err(|e| invalid_data(e.to_string()))?;
    store.validate_sqlite_source_metadata(envelope, source)?;
    let expected = database.read(|connection| {
        let bytes = raw_state(connection, &key, budget)?;
        bytes
            .map(|bytes| {
                serde_json::from_slice::<Progress>(&bytes).map_err(|e| invalid_data(e.to_string()))
            })
            .transpose()
    })?;
    let mut next = expected.clone().unwrap_or(Progress {
        cutoff,
        trusted_at,
        phase: Phase::Digests,
        cursor: None,
        state_cursor: String::new(),
        job: None,
        expired_days: BTreeSet::new(),
        retained_days: BTreeSet::new(),
        digest_namespace: None,
        digest_statistics_truncated: false,
        statistics_partial: false,
    });
    if next.cutoff != cutoff || next.trusted_at != trusted_at {
        return Err(invalid_data(
            "evidence GC progress retention boundary changed",
        ));
    }
    if let Some(job) = &next.job {
        validate_job(store, source, redaction, job)?;
    }
    let mut action = Action::None;
    let mut digest_pruned = 0;
    let mut reference_states = Vec::new();
    let root = format!(
        "{}/",
        database.namespace(
            &store
                .source_directory(source)
                .join(redaction.directory_name())
        )?
    );
    match next.phase {
        Phase::Digests => {
            let (rows, complete) = database.read(|connection| {
                sqlite_gc::scan_family(
                    connection,
                    Some(&root),
                    &mut next.cursor,
                    None,
                    budget,
                    |namespace| namespace.ends_with("/digests"),
                )
            })?;
            let mut deletion = Vec::new();
            for row in rows {
                if next.digest_namespace.as_ref() != Some(&row.cursor.namespace) {
                    if !next.digest_statistics_truncated {
                        digest_pruned += next.expired_days.difference(&next.retained_days).count();
                    }
                    next.expired_days.clear();
                    next.retained_days.clear();
                    next.digest_namespace = Some(row.cursor.namespace.clone());
                    next.digest_statistics_truncated = false;
                }
                let record: SourceSessionDigestRecord = serde_json::from_slice(&row.payload)
                    .map_err(|e| invalid_data(e.to_string()))?;
                group_digest_records_by_day(source, std::slice::from_ref(&record))?;
                if row.cursor.key != sqlite_record_key(&(record.thread_id(), record.range_start()))?
                    || row.cursor.sort_time != record.retention_through().timestamp_millis()
                {
                    return Err(invalid_data(
                        "digest GC database key or timestamp is invalid",
                    ));
                }
                if record.range_start().date_naive() < cutoff.date_naive()
                    && record.retention_through() < cutoff
                {
                    if !next.digest_statistics_truncated {
                        next.expired_days.insert(record.range_start().date_naive());
                    }
                    deletion.push(row);
                } else if !next.digest_statistics_truncated {
                    next.retained_days.insert(record.range_start().date_naive());
                }
                if next.expired_days.len() + next.retained_days.len() > MAX_DAY_STATISTICS {
                    // Statistics must never pin retention behind a huge marker.
                    // Keep data processing exact and expose a lower-bound count.
                    next.statistics_partial = true;
                    next.digest_statistics_truncated = true;
                    next.expired_days.clear();
                    next.retained_days.clear();
                }
            }
            action = Action::Delete(deletion);
            if complete {
                if !next.digest_statistics_truncated {
                    digest_pruned += next.expired_days.difference(&next.retained_days).count();
                }
                next.expired_days.clear();
                next.retained_days.clear();
                next.phase = Phase::Facts;
                next.cursor = None;
            }
        }
        Phase::Facts => {
            if next.job.is_none() {
                let prefix = format!(
                    "{}/",
                    database
                        .namespace(&store.source_fact_manifests_directory(source, redaction))?
                );
                let selected = database.read(|connection| {
                    use rusqlite::OptionalExtension;
                    let query = format!("SELECT state_key,payload FROM history_state WHERE {} AND state_key>?1 ORDER BY state_key LIMIT 1", sqlite_gc::prefix_predicate("state_key", &prefix));
                    connection.query_row(&query, [&next.state_cursor], |row| {
                        let bytes = row.get_ref(1)?.as_blob().map_err(|error| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Blob, Box::new(error)))?;
                        if bytes.len() as u64 > MAX_FACT_MANIFEST_BYTES { return Err(rusqlite::Error::InvalidQuery); }
                        Ok((row.get::<_, String>(0)?, bytes.to_vec()))
                    }).optional().map_err(database::sql_error)
                })?;
                if let Some((manifest_key, bytes)) = selected {
                    if bytes.len() as u64 > MAX_FACT_MANIFEST_BYTES {
                        return Err(invalid_data("GC fact manifest exceeds its size limit"));
                    }
                    budget.charge(bytes.len())?;
                    let manifest: ActiveFactManifest =
                        serde_json::from_slice(&bytes).map_err(|e| invalid_data(e.to_string()))?;
                    validate_active_manifest(
                        &manifest,
                        &store.profile_id,
                        source,
                        redaction,
                        &manifest.replica,
                        &manifest.thread_shard_key,
                    )?;
                    if manifest_key
                        != super::manifest_key(
                            store,
                            &database,
                            source,
                            redaction,
                            &manifest.thread_shard_key,
                        )?
                    {
                        return Err(invalid_data(
                            "GC active manifest key does not match its shard",
                        ));
                    }
                    if manifest.retained_since.is_some_and(|floor| floor >= cutoff) {
                        next.state_cursor = manifest_key;
                    } else {
                        next.job = Some(FactJob {
                            key: manifest_key,
                            expected: manifest,
                            replacement: FactBatchId::generate()?,
                            cursor: None,
                            seen_count: 0,
                            seen_days: BTreeSet::new(),
                            retained_count: 0,
                            retained_days: BTreeSet::new(),
                            retained_bytes: 0,
                            retire_namespace: None,
                            cancelled: false,
                        });
                    }
                } else {
                    next.phase = Phase::Staging;
                    next.state_cursor.clear();
                }
            } else {
                let job = next.job.as_mut().expect("fact GC job");
                let candidate = candidate_namespace(store, &database, source, redaction, job)?;
                if let Some(retired) = &job.retire_namespace {
                    let predicate = format!("namespace={}", sqlite_gc::sql_string(retired));
                    let (rows, complete) = database.read(|connection| {
                        sqlite_gc::read_rows(connection, &predicate, None, budget)
                    })?;
                    action = Action::Delete(rows);
                    if complete {
                        if !job.cancelled {
                            next.state_cursor = job.key.clone();
                        }
                        next.job = None;
                    }
                } else {
                    let current = database.read(|connection| {
                        let bytes = raw_state(connection, &job.key, budget)?;
                        bytes
                            .map(|bytes| {
                                serde_json::from_slice::<ActiveFactManifest>(&bytes)
                                    .map_err(|e| invalid_data(e.to_string()))
                            })
                            .transpose()
                    })?;
                    if current.as_ref() != Some(&job.expected) {
                        // A normal facts publication won the CAS while this job
                        // was idle. Retire the invisible candidate and revisit.
                        job.retire_namespace = Some(candidate);
                        job.cancelled = true;
                    } else {
                        let original = generation_namespace(
                            store,
                            &database,
                            source,
                            redaction,
                            &job.expected.thread_shard_key,
                            &job.expected.active_generation,
                        )?;
                        let predicate = format!("namespace={}", sqlite_gc::sql_string(&original));
                        let (rows, complete) = database.read(|connection| {
                            sqlite_gc::read_rows(
                                connection,
                                &predicate,
                                job.cursor.as_ref(),
                                budget,
                            )
                        })?;
                        job.cursor = rows.last().map(|r| r.cursor.clone()).or(job.cursor.clone());
                        let mut retained = Vec::new();
                        for row in rows {
                            let record: UsageEventFactRecord = serde_json::from_slice(&row.payload)
                                .map_err(|e| invalid_data(e.to_string()))?;
                            validate_fact_record_namespace(&record, &job.expected.replica)?;
                            if row.cursor.key != record.event_id().as_str()
                                || row.cursor.sort_time != record.occurred_at().timestamp_millis()
                                || job
                                    .expected
                                    .retained_since
                                    .is_some_and(|floor| record.occurred_at() < floor)
                            {
                                return Err(invalid_data(
                                    "fact GC record key, timestamp or retention floor is invalid",
                                ));
                            }
                            job.seen_count += 1;
                            job.seen_days.insert(record.occurred_at().date_naive());
                            if job.seen_count > MAX_FACT_GENERATION_RECORDS
                                || job.seen_count > job.expected.record_count
                            {
                                return Err(invalid_data(
                                    "fact GC generation exceeds its manifest record count",
                                ));
                            }
                            if record.occurred_at() >= cutoff {
                                job.retained_count += 1;
                                job.retained_days.insert(record.occurred_at().date_naive());
                                job.retained_bytes = job
                                    .retained_bytes
                                    .checked_add(row.payload.len() as u64)
                                    .ok_or_else(|| invalid_data("GC candidate size overflow"))?;
                                if job.retained_bytes > MAX_FACT_NAMESPACE_BYTES {
                                    return Err(invalid_data(
                                        "GC candidate exceeds its 512 MiB hard cap",
                                    ));
                                }
                                retained.push(row);
                            }
                        }
                        action = Action::Copy {
                            namespace: candidate.clone(),
                            rows: retained,
                        };
                        if complete {
                            if job.seen_count != job.expected.record_count
                                || job.seen_days.iter().copied().collect::<Vec<_>>()
                                    != job.expected.shard_days
                            {
                                return Err(invalid_data(
                                    "fact GC generation does not match its manifest",
                                ));
                            }
                            // Final pages are copied in this same short commit;
                            // no generation is visible before full validation.
                            if !matches!(&action, Action::Copy { rows, .. } if rows.is_empty()) {
                                // Publish on the next unit so its action need
                                // not combine an unbounded preparation scan.
                                job.cursor = Some(RowCursor {
                                    namespace: original,
                                    sort_time: i64::MAX,
                                    key: String::new(),
                                });
                            } else {
                                let changed = job.retained_count != job.seen_count;
                                let replacement = ActiveFactManifest {
                                    active_generation: if changed {
                                        job.replacement.clone()
                                    } else {
                                        job.expected.active_generation.clone()
                                    },
                                    retained_since: Some(cutoff),
                                    shard_days: job.retained_days.iter().copied().collect(),
                                    record_count: job.retained_count,
                                    ..job.expected.clone()
                                };
                                action = Action::Publish {
                                    key: job.key.clone(),
                                    expected: Box::new(job.expected.clone()),
                                    replacement: Box::new(replacement),
                                };
                                job.retire_namespace =
                                    Some(if changed { original } else { candidate });
                                job.cursor = None;
                            }
                        }
                    }
                }
            }
        }
        Phase::Staging => {
            let prefix = format!(
                "{}/",
                database.namespace(&store.source_fact_staging_directory(source, redaction))?
            );
            let selected = database.read(|connection| {
                use rusqlite::OptionalExtension;
                let query = format!("SELECT state_key FROM history_state WHERE {} AND substr(state_key,-11)='/batch.json' AND state_key>?1 ORDER BY state_key LIMIT 1", sqlite_gc::prefix_predicate("state_key", &prefix));
                connection.query_row(&query, [&next.state_cursor], |row| row.get::<_,String>(0)).optional().map_err(database::sql_error)
            })?;
            if let Some(batch_key) = selected {
                let batch_name = batch_key
                    .strip_prefix(&prefix)
                    .and_then(|name| name.strip_suffix("/batch.json"))
                    .ok_or_else(|| invalid_data("GC staging key is invalid"))?;
                let batch_id = batch_name
                    .parse::<FactBatchId>()
                    .map_err(|e| invalid_data(e.to_string()))?;
                let descriptor_bytes = database
                    .read(|connection| raw_state(connection, &batch_key, budget))?
                    .ok_or_else(|| invalid_data("GC staged descriptor disappeared"))?;
                if descriptor_bytes.len() as u64 > MAX_FACT_MANIFEST_BYTES {
                    return Err(invalid_data("GC staged descriptor exceeds its size limit"));
                }
                let descriptor: StagedFactBatch = serde_json::from_slice(&descriptor_bytes)
                    .map_err(|e| invalid_data(e.to_string()))?;
                validate_staged_batch(
                    descriptor,
                    &store
                        .source_fact_staging_directory(source, redaction)
                        .join(batch_name)
                        .join(STAGED_BATCH_FILE),
                    &store.profile_id,
                    source,
                    redaction,
                    &batch_id,
                )?;
                let timestamp_key = staged_key(
                    store,
                    &database,
                    source,
                    redaction,
                    &batch_id,
                    "staged-at.json",
                )?;
                let timestamp_bytes = database
                    .read(|connection| raw_state(connection, &timestamp_key, budget))?
                    .ok_or_else(|| {
                        invalid_data("staged fact batch has no central staging timestamp")
                    })?;
                let timestamp: DateTime<Utc> = serde_json::from_slice(&timestamp_bytes)
                    .map_err(|e| invalid_data(e.to_string()))?;
                if timestamp
                    < trusted_at
                        .checked_sub_signed(Duration::hours(FACT_STAGING_TTL_HOURS))
                        .unwrap_or(DateTime::<Utc>::MIN_UTC)
                {
                    let publication_key = staged_key(
                        store,
                        &database,
                        source,
                        redaction,
                        &batch_id,
                        STAGED_PUBLICATION_FILE,
                    )?;
                    let mut deletion = vec![
                        (batch_key.clone(), descriptor_bytes),
                        (timestamp_key, timestamp_bytes),
                    ];
                    if let Some(bytes) = database
                        .read(|connection| raw_state(connection, &publication_key, budget))?
                    {
                        deletion.push((publication_key, bytes));
                    }
                    action = Action::DeleteStates(deletion);
                }
                next.state_cursor = batch_key;
            } else {
                next.phase = Phase::Orphans;
                next.cursor = None;
            }
        }
        Phase::Orphans => {
            let prefix = format!(
                "{}/",
                database.namespace(&store.source_facts_directory(source, redaction))?
            );
            let namespace =
                if let Some(cursor) = next.cursor.as_ref().filter(|c| c.sort_time != i64::MAX) {
                    Some(cursor.namespace.clone())
                } else {
                    let namespace = database.read(|connection| {
                        sqlite_gc::next_namespace(
                            connection,
                            Some(&prefix),
                            next.cursor.as_ref().map_or("", |c| c.namespace.as_str()),
                        )
                    })?;
                    if let Some(namespace) = &namespace {
                        budget.charge(namespace.len())?;
                        next.cursor = Some(RowCursor {
                            namespace: namespace.clone(),
                            sort_time: i64::MIN,
                            key: String::new(),
                        });
                    }
                    namespace
                };
            if let Some(namespace) = namespace {
                let relative = namespace
                    .strip_prefix(&prefix)
                    .ok_or_else(|| invalid_data("GC fact artifact namespace is invalid"))?;
                let (shard_name, generation_name) = relative
                    .split_once('/')
                    .ok_or_else(|| invalid_data("GC fact artifact path is invalid"))?;
                let shard = shard_name
                    .parse::<ThreadShardKey>()
                    .map_err(|e| invalid_data(e.to_string()))?;
                let generation = generation_name
                    .parse::<FactBatchId>()
                    .map_err(|e| invalid_data(e.to_string()))?;
                let manifest_key =
                    super::manifest_key(store, &database, source, redaction, &shard)?;
                let active_bytes =
                    database.read(|connection| raw_state(connection, &manifest_key, budget))?;
                let active = active_bytes
                    .as_ref()
                    .map(|bytes| {
                        serde_json::from_slice::<ActiveFactManifest>(bytes)
                            .map_err(|e| invalid_data(e.to_string()))
                    })
                    .transpose()?;
                reference_states.push((manifest_key, active_bytes));
                if let Some(manifest) = &active {
                    validate_active_manifest(
                        manifest,
                        &store.profile_id,
                        source,
                        redaction,
                        &manifest.replica,
                        &shard,
                    )?;
                }
                let descriptor_key = staged_key(
                    store,
                    &database,
                    source,
                    redaction,
                    &generation,
                    STAGED_BATCH_FILE,
                )?;
                let staged_bytes =
                    database.read(|connection| raw_state(connection, &descriptor_key, budget))?;
                let staged = staged_bytes
                    .as_ref()
                    .map(|bytes| {
                        serde_json::from_slice::<StagedFactBatch>(bytes)
                            .map_err(|e| invalid_data(e.to_string()))
                    })
                    .transpose()?;
                reference_states.push((descriptor_key, staged_bytes));
                let staged_match = if let Some(descriptor) = staged {
                    let descriptor = validate_staged_batch(
                        descriptor,
                        &store
                            .source_fact_staging_directory(source, redaction)
                            .join(generation.as_str())
                            .join(STAGED_BATCH_FILE),
                        &store.profile_id,
                        source,
                        redaction,
                        &generation,
                    )?;
                    descriptor.thread_shard_key == shard
                } else {
                    false
                };
                if active.is_some_and(|manifest| manifest.active_generation == generation)
                    || staged_match
                {
                    // One reference check skips an entire live immutable generation.
                    next.cursor = Some(RowCursor {
                        namespace,
                        sort_time: i64::MAX,
                        key: String::new(),
                    });
                } else {
                    let predicate = format!("namespace={}", sqlite_gc::sql_string(&namespace));
                    let (rows, complete) = database.read(|connection| {
                        sqlite_gc::read_rows(connection, &predicate, next.cursor.as_ref(), budget)
                    })?;
                    if complete {
                        next.cursor = Some(RowCursor {
                            namespace,
                            sort_time: i64::MAX,
                            key: String::new(),
                        });
                    } else {
                        next.cursor = rows.last().map(|r| r.cursor.clone()).or(next.cursor);
                    }
                    action = Action::Delete(rows);
                }
            } else {
                next.phase = Phase::Done;
            }
        }
        Phase::Done => {}
    }
    if serde_json::to_vec(&next)
        .map_err(|e| invalid_data(e.to_string()))?
        .len() as u64
        > MAX_PROGRESS_BYTES
    {
        return Err(invalid_data("evidence GC progress exceeds its size limit"));
    }
    Ok(PreparedEvidenceGc {
        key,
        expected,
        next,
        action,
        digest_pruned,
        references: reference_states,
        source_metadata: (metadata_key, metadata_bytes),
        source: source.clone(),
        redaction,
    })
}

impl PreparedEvidenceGc {
    pub(crate) fn publish(
        &self,
        store: &SourceHistoryStore,
        database: &database::HistoryDatabase,
        connection: &rusqlite::Connection,
    ) -> io::Result<EvidenceGcReport> {
        if sqlite_state_bounded::<Progress>(connection, &self.key, MAX_PROGRESS_BYTES, None)?
            != self.expected
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "evidence GC progress changed after preparation",
            ));
        }
        let metadata_unchanged: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM history_state WHERE state_key=?1 AND payload=?2)",
                rusqlite::params![self.source_metadata.0, self.source_metadata.1],
                |row| row.get(0),
            )
            .map_err(database::sql_error)?;
        if !metadata_unchanged {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "GC source metadata changed or was purged after preparation",
            ));
        }
        for (key, expected) in &self.references {
            let unchanged: bool = if let Some(payload) = expected {
                connection.query_row("SELECT EXISTS(SELECT 1 FROM history_state WHERE state_key=?1 AND payload=?2)", rusqlite::params![key, payload], |row| row.get(0)).map_err(database::sql_error)?
            } else {
                connection
                    .query_row(
                        "SELECT NOT EXISTS(SELECT 1 FROM history_state WHERE state_key=?1)",
                        [key],
                        |row| row.get(0),
                    )
                    .map_err(database::sql_error)?
            };
            if !unchanged {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "GC artifact references changed after preparation",
                ));
            }
        }
        // Any publication which changed active facts invalidates a prepared
        // page, including a retained-since-only update at an identical cursor.
        if let Some(job) = self
            .expected
            .as_ref()
            .and_then(|p| p.job.as_ref())
            .filter(|j| j.retire_namespace.is_none())
            && sqlite_state_bounded::<ActiveFactManifest>(
                connection,
                &job.key,
                MAX_FACT_MANIFEST_BYTES,
                None,
            )? != Some(job.expected.clone())
            && !matches!(self.action, Action::None)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "fact GC active version changed after preparation",
            ));
        }
        let mut visible_changed = false;
        let mut pruned = self.digest_pruned;
        match &self.action {
            Action::None => {}
            Action::Delete(rows) => {
                sqlite_gc::delete_rows(connection, rows)?;
                visible_changed = self
                    .expected
                    .as_ref()
                    .is_none_or(|p| p.phase == Phase::Digests)
                    && !rows.is_empty();
            }
            Action::Copy { namespace, rows } => {
                for row in rows {
                    connection.execute("INSERT INTO history_records(namespace,record_key,sort_time,payload) VALUES(?1,?2,?3,?4)", rusqlite::params![namespace, row.cursor.key, row.cursor.sort_time, row.payload]).map_err(database::sql_error)?;
                }
            }
            Action::Publish {
                key,
                expected,
                replacement,
            } => {
                if sqlite_state_bounded::<ActiveFactManifest>(
                    connection,
                    key,
                    MAX_FACT_MANIFEST_BYTES,
                    None,
                )? != Some(expected.as_ref().clone())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "fact GC compare-and-swap failed",
                    ));
                }
                // GC has a separate, tightly bounded physical metadata budget.
                // It pays only the retainedSince field's actual serialized
                // growth; ordinary new facts still consume the original cap.
                let old_size = serde_json::to_vec(&expected.retained_since)
                    .map_err(|e| invalid_data(e.to_string()))?
                    .len() as u64;
                let new_size = serde_json::to_vec(&replacement.retained_since)
                    .map_err(|e| invalid_data(e.to_string()))?
                    .len() as u64;
                let current = bookkeeping_allowance(
                    store,
                    database,
                    connection,
                    &self.source,
                    self.redaction,
                )?;
                let next = if new_size >= old_size {
                    let growth = new_size - old_size;
                    if growth > MAX_BOOKKEEPING_PER_MANIFEST {
                        return Err(invalid_data(
                            "GC retention floor exceeds its per-manifest metadata allowance",
                        ));
                    }
                    current
                        .checked_add(growth)
                        .ok_or_else(|| invalid_data("GC bookkeeping allowance overflow"))?
                } else {
                    current
                        .checked_sub(old_size - new_size)
                        .ok_or_else(|| invalid_data("GC bookkeeping allowance underflow"))?
                };
                if next > MAX_BOOKKEEPING_BYTES {
                    return Err(invalid_data(
                        "GC bookkeeping allowance exceeds its independent hard cap",
                    ));
                }
                if next != current {
                    database::set_state(
                        connection,
                        &bookkeeping_key(store, database, &self.source, self.redaction)?,
                        &next,
                    )?;
                }
                database::set_state(connection, key, replacement)?;
                store.advance_facts_projection_revision(connection)?;
                visible_changed = expected.record_count != replacement.record_count;
                pruned += expected
                    .shard_days
                    .iter()
                    .filter(|day| !replacement.shard_days.contains(day))
                    .count();
            }
            Action::DeleteStates(states) => {
                for (key, payload) in states {
                    if connection
                        .execute(
                            "DELETE FROM history_state WHERE state_key=?1 AND payload=?2",
                            rusqlite::params![key, payload],
                        )
                        .map_err(database::sql_error)?
                        != 1
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "GC staged state changed after preparation",
                        ));
                    }
                }
            }
        }
        let complete = self.next.phase == Phase::Done;
        if complete {
            database::delete_state(connection, &self.key)?;
        } else {
            database::set_state(connection, &self.key, &self.next)?;
        }
        // The cap excludes only the managed, separately bounded invisible
        // candidate/retirement generation; all other active/staged data count.
        Ok(EvidenceGcReport {
            complete,
            visible_changed,
            pruned,
            statistics_partial: self.next.statistics_partial,
        })
    }
}
