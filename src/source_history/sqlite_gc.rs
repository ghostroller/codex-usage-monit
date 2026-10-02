//! Resumable retention: decode a bounded page before acquiring the SQL writer.
use super::*;
use std::collections::BTreeSet;

const PROGRESS_FILE: &str = "garbage-collection-progress.json";
const MAX_GC_UNIT_RECORDS: usize = 4096;
const MAX_GC_UNIT_BYTES: u64 = MAX_SHARD_FILE_BYTES;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Progress {
    profile: HistoryProfileId,
    redactions: Vec<RedactionProfile>,
    cutoff: DateTime<Utc>,
    trusted_at: DateTime<Utc>,
    pruning_deferred: bool,
    core_done: bool,
    cursor: Option<RowCursor>,
    source_cursor: Option<String>,
    #[serde(default)]
    working_source: Option<String>,
    #[serde(default)]
    paused: bool,
    anchor: Option<Anchor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Anchor {
    namespace: String,
    earliest: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RowCursor {
    pub namespace: String,
    pub sort_time: i64,
    pub key: String,
}

#[derive(Clone, Debug)]
pub(super) struct RawRow {
    pub cursor: RowCursor,
    pub payload: Vec<u8>,
}

pub(crate) struct Budget {
    rows_left: usize,
    bytes_left: u64,
    pub rows: usize,
    pub bytes: u64,
}

impl Budget {
    fn new() -> Self {
        Self::with_limits(MAX_GC_UNIT_RECORDS, MAX_GC_UNIT_BYTES)
    }
    pub(super) fn with_limits(rows: usize, bytes: u64) -> Self {
        Self {
            rows_left: rows,
            bytes_left: bytes,
            rows: 0,
            bytes: 0,
        }
    }
    pub(super) fn take(&mut self, bytes: usize) -> io::Result<bool> {
        if bytes as u64 > MAX_SHARD_FILE_BYTES {
            return Err(invalid_data(
                "GC record exceeds the persisted record size limit",
            ));
        }
        if self.rows_left == 0 || bytes as u64 > self.bytes_left {
            if self.rows == 0 {
                return Err(invalid_data("GC record exceeds its work-unit byte budget"));
            }
            return Ok(false);
        }
        self.rows_left -= 1;
        self.bytes_left -= bytes as u64;
        self.rows += 1;
        self.bytes += bytes as u64;
        Ok(true)
    }
    pub(super) fn capacity(&self) -> usize {
        self.rows_left
    }
    pub(super) fn charge(&mut self, bytes: usize) -> io::Result<()> {
        if !self.take(bytes)? {
            return Err(invalid_data(
                "GC metadata exceeds its remaining work-unit budget",
            ));
        }
        Ok(())
    }
}

fn progress_key(
    database: &database::HistoryDatabase,
    store: &SourceHistoryStore,
    redactions: &[RedactionProfile],
) -> io::Result<String> {
    let label = redactions
        .iter()
        .map(|r| r.directory_name())
        .collect::<Vec<_>>()
        .join("+");
    database.namespace(&store.profile_directory().join(label).join(PROGRESS_FILE))
}

pub(super) fn has_pending(
    connection: &rusqlite::Connection,
    database: &database::HistoryDatabase,
    store: &SourceHistoryStore,
    redaction: RedactionProfile,
) -> io::Result<bool> {
    let key = progress_key(database, store, &[redaction])?;
    Ok(
        sqlite_state_bounded::<Progress>(connection, &key, MAX_METADATA_FILE_BYTES, None)?
            .is_some_and(|p| !p.paused),
    )
}

pub(super) fn read_rows(
    connection: &rusqlite::Connection,
    predicate: &str,
    cursor: Option<&RowCursor>,
    budget: &mut Budget,
) -> io::Result<(Vec<RawRow>, bool)> {
    let query = format!(
        "SELECT namespace,sort_time,record_key,payload FROM history_records WHERE ({predicate}) AND (namespace,sort_time,record_key)>(?1,?2,?3) ORDER BY namespace,sort_time,record_key LIMIT ?4"
    );
    let mut statement = connection.prepare(&query).map_err(database::sql_error)?;
    let mut rows = statement
        .query(rusqlite::params![
            cursor.map_or("", |c| c.namespace.as_str()),
            cursor.map_or(i64::MIN, |c| c.sort_time),
            cursor.map_or("", |c| c.key.as_str()),
            budget.rows_left.saturating_add(1) as i64
        ])
        .map_err(database::sql_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(database::sql_error)? {
        let payload = row
            .get_ref(3)
            .map_err(database::sql_error)?
            .as_blob()
            .map_err(|e| invalid_data(e.to_string()))?;
        if !budget.take(payload.len())? {
            if result.is_empty() && budget.capacity() > 0 {
                return Err(invalid_data(
                    "GC record cannot fit beside its bounded metadata in one work unit",
                ));
            }
            return Ok((result, false));
        }
        result.push(RawRow {
            cursor: RowCursor {
                namespace: row.get(0).map_err(database::sql_error)?,
                sort_time: row.get(1).map_err(database::sql_error)?,
                key: row.get(2).map_err(database::sql_error)?,
            },
            payload: payload.to_vec(),
        });
    }
    Ok((result, true))
}

pub(super) fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(super) fn prefix_predicate(column: &str, prefix: &str) -> String {
    let upper = format!(
        "{}0",
        prefix
            .strip_suffix('/')
            .expect("slash-terminated SQL prefix")
    );
    format!(
        "{column}>={} AND {column}<{}",
        sql_string(prefix),
        sql_string(&upper)
    )
}

pub(super) fn next_namespace(
    connection: &rusqlite::Connection,
    prefix: Option<&str>,
    after: &str,
) -> io::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let bounds = prefix.map_or_else(
        || "1".to_owned(),
        |prefix| prefix_predicate("namespace", prefix),
    );
    connection.query_row(&format!("SELECT namespace FROM history_records WHERE namespace>?1 AND {bounds} ORDER BY namespace LIMIT 1"), [after], |row| row.get(0)).optional().map_err(database::sql_error)
}

/// Namespace seeks also consume the shared allowance. Filtering never walks
/// every row of an unrelated namespace or all current rows to find expiry.
pub(super) fn scan_family(
    connection: &rusqlite::Connection,
    prefix: Option<&str>,
    cursor: &mut Option<RowCursor>,
    cutoff: Option<i64>,
    budget: &mut Budget,
    eligible: impl Fn(&str) -> bool,
) -> io::Result<(Vec<RawRow>, bool)> {
    let mut result = Vec::new();
    while budget.capacity() > 0 {
        let namespace = if let Some(current) = cursor.as_ref().filter(|c| c.sort_time != i64::MAX) {
            current.namespace.clone()
        } else {
            let next = next_namespace(
                connection,
                prefix,
                cursor.as_ref().map_or("", |c| c.namespace.as_str()),
            )?;
            let Some(next) = next else {
                return Ok((result, true));
            };
            if next.len() > 8192 {
                return Err(invalid_data("GC namespace exceeds its size limit"));
            }
            if !budget.take(next.len())? {
                return Ok((result, false));
            }
            *cursor = Some(RowCursor {
                namespace: next.clone(),
                sort_time: i64::MIN,
                key: String::new(),
            });
            next
        };
        if !eligible(&namespace) {
            *cursor = Some(RowCursor {
                namespace,
                sort_time: i64::MAX,
                key: String::new(),
            });
            continue;
        }
        let predicate = format!(
            "namespace={}{}",
            sql_string(&namespace),
            cutoff.map_or_else(String::new, |at| format!(" AND sort_time<{at}"))
        );
        let (rows, complete) = read_rows(connection, &predicate, cursor.as_ref(), budget)?;
        if complete {
            *cursor = Some(RowCursor {
                namespace,
                sort_time: i64::MAX,
                key: String::new(),
            });
        } else {
            *cursor = rows.last().map(|r| r.cursor.clone()).or(cursor.clone());
        }
        result.extend(rows);
        if !complete {
            return Ok((result, false));
        }
    }
    Ok((result, false))
}

pub(super) fn delete_rows(connection: &rusqlite::Connection, rows: &[RawRow]) -> io::Result<()> {
    for row in rows {
        let removed = connection.execute("DELETE FROM history_records WHERE namespace=?1 AND record_key=?2 AND sort_time=?3 AND payload=?4", rusqlite::params![row.cursor.namespace, row.cursor.key, row.cursor.sort_time, row.payload]).map_err(database::sql_error)?;
        if removed != 1 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "GC record changed after preparation",
            ));
        }
    }
    Ok(())
}

pub(super) fn publication_changed(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    connection: &rusqlite::Connection,
) -> io::Result<()> {
    let key = database.namespace(
        &store
            .profile_directory()
            .join(GARBAGE_COLLECTION_PUBLICATION_FILE),
    )?;
    let next = database::state::<u64>(connection, &key)?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid_data("history GC publication revision overflow"))?;
    database::set_state(connection, &key, &next)
}

pub(super) fn collect_unit(
    store: &SourceHistoryStore,
    observed_at: DateTime<Utc>,
    redactions: &[RedactionProfile],
    fence: impl Fn() -> io::Result<()>,
) -> io::Result<SourceHistoryGcReport> {
    collect_unit_with_budget(store, observed_at, redactions, Budget::new(), fence)
}

pub(super) fn collect_unit_with_budget(
    store: &SourceHistoryStore,
    observed_at: DateTime<Utc>,
    redactions: &[RedactionProfile],
    mut budget: Budget,
    fence: impl Fn() -> io::Result<()>,
) -> io::Result<SourceHistoryGcReport> {
    let database = store.sqlite_database().expect("SQLite backend");
    let key = progress_key(&database, store, redactions)?;
    let result = collect_prepared_unit(
        store,
        &database,
        observed_at,
        redactions,
        &key,
        &mut budget,
        &fence,
    );
    if result
        .as_ref()
        .is_err_and(|error| error.kind() != io::ErrorKind::WouldBlock)
    {
        // Failed pages remain resumable but obey the existing failure throttle.
        database.write(|connection| {
            fence()?;
            if let Some(mut progress) =
                sqlite_state_bounded::<Progress>(connection, &key, MAX_METADATA_FILE_BYTES, None)?
            {
                progress.paused = true;
                database::set_state(connection, &key, &progress)?;
            }
            for &redaction in redactions {
                let schedule_key = database.namespace(
                    &store
                        .profile_directory()
                        .join(redaction.directory_name())
                        .join(GARBAGE_COLLECTION_SCHEDULE_FILE),
                )?;
                database::set_state(
                    connection,
                    &schedule_key,
                    &GarbageCollectionSchedule::new(
                        store.profile_id.clone(),
                        redaction,
                        observed_at,
                    ),
                )?;
            }
            fence()
        })?;
    }
    result
}

fn collect_prepared_unit(
    store: &SourceHistoryStore,
    database: &database::HistoryDatabase,
    observed_at: DateTime<Utc>,
    redactions: &[RedactionProfile],
    key: &str,
    budget: &mut Budget,
    fence: &impl Fn() -> io::Result<()>,
) -> io::Result<SourceHistoryGcReport> {
    let sources = store.list_source_metadata()?;
    let expected = database.read(|connection| {
        sqlite_state_bounded::<Progress>(connection, key, MAX_METADATA_FILE_BYTES, None)
    })?;
    let clock_key = database.namespace(&store.profile_directory().join(RETENTION_CLOCK_FILE))?;
    let expected_clock = database.read(|connection| {
        sqlite_state_bounded::<RetentionClockEnvelope>(
            connection,
            &clock_key,
            MAX_METADATA_FILE_BYTES,
            None,
        )
    })?;
    if let Some(clock) = &expected_clock {
        if clock.format_version != RETENTION_CLOCK_FORMAT_VERSION
            || clock.profile_id != store.profile_id
        {
            return Err(invalid_data("retention clock database envelope is invalid"));
        }
        clock.clock.validate()?;
    }
    let mut prepared_clock = None;
    let mut progress = if let Some(progress) = &expected {
        let expected_cutoff = progress
            .trusted_at
            .checked_sub_signed(Duration::days(SOURCE_HISTORY_RETENTION_DAYS))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc();
        if progress.profile != store.profile_id
            || progress.redactions != redactions
            || progress.cutoff != expected_cutoff
        {
            return Err(invalid_data(
                "GC progress namespace or retention boundary is invalid",
            ));
        }
        if progress.anchor.is_none()
            && expected_clock
                .as_ref()
                .is_none_or(|clock| clock.clock.trusted_at < progress.trusted_at)
        {
            return Err(invalid_data(
                "resumable GC progress has no matching trusted retention clock",
            ));
        }
        progress.clone()
    } else {
        let current = expected_clock
            .as_ref()
            .map(|envelope| {
                if envelope.format_version != RETENTION_CLOCK_FORMAT_VERSION
                    || envelope.profile_id != store.profile_id
                {
                    return Err(invalid_data("retention clock database envelope is invalid"));
                }
                envelope.clock.validate()?;
                Ok(envelope.clock)
            })
            .transpose()?;
        let (clock, pruning_deferred) = next_retention_clock(current, None, observed_at);
        if current.is_some() {
            prepared_clock = Some(clock);
        }
        Progress {
            profile: store.profile_id.clone(),
            redactions: redactions.to_vec(),
            cutoff: clock
                .trusted_at
                .checked_sub_signed(Duration::days(SOURCE_HISTORY_RETENTION_DAYS))
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight")
                .and_utc(),
            trusted_at: clock.trusted_at,
            pruning_deferred,
            core_done: false,
            cursor: None,
            source_cursor: None,
            working_source: None,
            paused: false,
            anchor: current.is_none().then_some(Anchor {
                namespace: String::new(),
                earliest: None,
            }),
        }
    };
    progress.paused = false;
    let mut deletion = Vec::new();
    let mut pruned_days = BTreeSet::new();
    let mut evidence = None;
    let mut source_context = None;
    let began_evidence = progress.anchor.is_none() && progress.core_done;
    if let Some(anchor) = &mut progress.anchor {
        // Index seeks skip the remaining rows of each namespace. Neither a
        // full-profile min scan nor unbounded DISTINCT enumeration is needed.
        let complete = database.read(|connection| {
            use rusqlite::OptionalExtension;
            while budget.capacity() > 0 {
                let namespace: Option<String> = connection.query_row("SELECT namespace FROM history_records WHERE namespace>?1 ORDER BY namespace LIMIT 1", [&anchor.namespace], |row| row.get(0)).optional().map_err(database::sql_error)?;
                let Some(namespace) = namespace else { return Ok(true); };
                if namespace.len() > 8192 { return Err(invalid_data("GC namespace exceeds its size limit")); }
                if !budget.take(namespace.len() + 8)? { return Ok(false); }
                let timestamp: i64 = connection.query_row("SELECT min(sort_time) FROM history_records WHERE namespace=?1", [&namespace], |row| row.get(0)).map_err(database::sql_error)?;
                let timestamp = DateTime::<Utc>::from_timestamp_millis(timestamp).ok_or_else(|| invalid_data("invalid persisted history timestamp"))?.date_naive().and_hms_opt(0,0,0).expect("midnight").and_utc();
                anchor.earliest = Some(anchor.earliest.map_or(timestamp, |at| at.min(timestamp)));
                anchor.namespace = namespace;
            }
            Ok(false)
        })?;
        if complete {
            let (clock, deferred) = next_retention_clock(
                expected_clock.as_ref().map(|e| e.clock),
                anchor.earliest.map(|at| at.min(observed_at)),
                observed_at,
            );
            prepared_clock = Some(clock);
            progress.trusted_at = clock.trusted_at;
            progress.cutoff = clock
                .trusted_at
                .checked_sub_signed(Duration::days(SOURCE_HISTORY_RETENTION_DAYS))
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight")
                .and_utc();
            progress.pruning_deferred = deferred;
            progress.anchor = None;
        }
    } else if !progress.core_done {
        let source_prefixes = sources
            .iter()
            .flat_map(|source| {
                redactions.iter().map(move |redaction| {
                    format!(
                        "sources/{}/{}/",
                        source.source_id().as_str(),
                        redaction.directory_name()
                    )
                })
            })
            .collect::<Vec<_>>();
        let (rows, complete) = database.read(|connection| {
            scan_family(
                connection,
                None,
                &mut progress.cursor,
                Some(progress.cutoff.timestamp_millis()),
                budget,
                |namespace| {
                    namespace == "account"
                        || (source_prefixes
                            .iter()
                            .any(|prefix| namespace.starts_with(prefix))
                            && (namespace.ends_with("/buckets")
                                || namespace.ends_with("/weekly")
                                || namespace.ends_with("/retained-quota")))
                },
            )
        })?;
        let account = database.namespace(&store.account_directory())?;
        for row in &rows {
            let actual = if row.cursor.namespace == account
                || row.cursor.namespace.ends_with("/retained-quota")
            {
                let value: QuotaPoint = serde_json::from_slice(&row.payload)
                    .map_err(|e| invalid_data(e.to_string()))?;
                validate_account_quota_point(&value)?;
                if row.cursor.key != sqlite_quota_key(&value)? {
                    return Err(invalid_data(
                        "GC quota database key does not match its record",
                    ));
                }
                value.observed_at
            } else if row.cursor.namespace.ends_with("/buckets") {
                let value: SourceBucketRecord = serde_json::from_slice(&row.payload)
                    .map_err(|e| invalid_data(e.to_string()))?;
                value.validate()?;
                value.starts_at
            } else {
                let value: SourceWeeklyRecord = serde_json::from_slice(&row.payload)
                    .map_err(|e| invalid_data(e.to_string()))?;
                value.validate()?;
                value.observed_at
            };
            if actual.timestamp_millis() != row.cursor.sort_time {
                return Err(invalid_data(
                    "history database ordering timestamp does not match its record",
                ));
            }
            pruned_days.insert((row.cursor.namespace.clone(), actual.date_naive()));
        }
        progress.core_done = complete;
        deletion = rows;
    } else {
        if progress.working_source.as_ref().is_some_and(|working| {
            !sources.iter().any(|source| {
                redactions.iter().any(|redaction| {
                    format!(
                        "{}/{}",
                        source.source_id().as_str(),
                        redaction.directory_name()
                    ) == *working
                })
            })
        }) {
            progress.working_source = None;
        }
        let next = sources
            .iter()
            .flat_map(|source| redactions.iter().map(move |redaction| (source, *redaction)))
            .filter_map(|(source, redaction)| {
                let label = format!(
                    "{}/{}",
                    source.source_id().as_str(),
                    redaction.directory_name()
                );
                (progress.working_source.as_ref().map_or_else(
                    || {
                        progress
                            .source_cursor
                            .as_ref()
                            .is_none_or(|cursor| label > *cursor)
                    },
                    |working| label == *working,
                ))
                .then_some((label, source, redaction))
            })
            .min_by(|left, right| left.0.cmp(&right.0));
        if let Some((label, source, redaction)) = next {
            progress.working_source = Some(label.clone());
            evidence = Some(session_evidence::prepare_gc_evidence_unit(
                store,
                source.source_id(),
                redaction,
                progress.cutoff,
                progress.trusted_at,
                budget,
            )?);
            source_context = Some((label, source, redaction));
        }
    }
    // All JSON preparation above has completed before BEGIN IMMEDIATE.
    fence()?;
    database.write(|connection| {
        fence()?;
        if sqlite_state_bounded::<Progress>(connection, key, MAX_METADATA_FILE_BYTES, None)? != expected { return Err(io::Error::new(io::ErrorKind::WouldBlock, "GC progress changed after preparation")); }
        if let Some(clock) = prepared_clock {
            let current = sqlite_state_bounded::<RetentionClockEnvelope>(connection, &clock_key, MAX_METADATA_FILE_BYTES, None)?;
            if current != expected_clock { return Err(io::Error::new(io::ErrorKind::WouldBlock, "retention clock changed after preparation")); }
            database::set_state(connection, &clock_key, &RetentionClockEnvelope { format_version: RETENTION_CLOCK_FORMAT_VERSION, profile_id: store.profile_id.clone(), clock })?;
        }
        delete_rows(connection, &deletion)?;
        let mut visible = !deletion.is_empty();
        let mut pruned = 0;
        let mut statistics_partial = false;
        for (namespace, day) in &pruned_days {
            let start = day.and_hms_opt(0,0,0).expect("midnight").and_utc().timestamp_millis();
            let retained: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM history_records WHERE namespace=?1 AND sort_time>=?2 AND sort_time<?3)", rusqlite::params![namespace, start, start.saturating_add(86_400_000)], |row| row.get(0)).map_err(database::sql_error)?;
            if !retained { pruned += 1; }
        }
        let mut finished = began_evidence && source_context.is_none();
        if let (Some(prepared), Some((label, source, redaction))) = (&evidence, &source_context) {
            let report = prepared.publish(store, database, connection)?;
            visible |= report.visible_changed;
            pruned += report.pruned;
            statistics_partial |= report.statistics_partial;
            if report.complete { progress.source_cursor = Some(label.clone()); progress.working_source = None; }
            if report.visible_changed && source.kind() == SourceKind::Ssh { store.advance_remote_history_projection_revision(connection, source.source_id(), *redaction)?; }
            finished = false;
        }
        if !deletion.is_empty() {
            for source in sources.iter().filter(|s| s.kind() == SourceKind::Ssh) {
                for &redaction in redactions {
                    let prefix = format!("{}/", database.namespace(&store.source_directory(source.source_id()).join(redaction.directory_name()))?);
                    if deletion.iter().any(|row| row.cursor.namespace.starts_with(&prefix)) { store.advance_remote_history_projection_revision(connection, source.source_id(), redaction)?; }
                }
            }
        }
        if visible { publication_changed(store, database, connection)?; }
        if finished { database::delete_state(connection, key)?; } else { database::set_state(connection, key, &progress)?; }
        fence()?;
        Ok(SourceHistoryGcReport { shards_pruned: pruned, pruning_deferred: progress.pruning_deferred, trusted_at: progress.anchor.is_none().then_some(progress.trusted_at), work_pending: !finished, records_examined: budget.rows, decoded_bytes: budget.bytes, pruning_statistics_partial: statistics_partial })
    })
}
