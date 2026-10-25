use super::*;
use std::sync::{Arc, Barrier};
use std::thread;

fn fill_pending(config: &CollectConfig, count: usize) {
    let records = (0..count)
        .map(|index| record(&format!("thread-{index:03}"), "event", 1))
        .collect::<Vec<_>>();
    prepare_delete(config, "original", &records).unwrap();
}

#[test]
fn deleted_sessions_pending_capacity_rejects_the_whole_batch_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    fill_pending(&config, MAX_PENDING_RECEIPTS);
    let (_, ledger_directory, _) = location(&config).unwrap();
    let path = ledger_directory.join(LAYOUT.data_file_name);
    let before = fs::read(&path).unwrap();

    let error = prepare_delete(
        &config,
        "overflow",
        &[
            record("thread-000", "new-event", 7),
            record("extra", "event", 1),
        ],
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unresolved deletion member budget")
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        pending_deletion_receipts(&config).unwrap().len(),
        MAX_PENDING_RECEIPTS,
        "a rejected batch must leave the complete prior inventory readable"
    );
    with_database(&config, false, |connection| {
        for table in ["operations", "outcomes"] {
            let column = if table == "operations" {
                "id"
            } else {
                "operation_id"
            };
            let count: i64 = connection.query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}='overflow'"),
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0, "no partial operation may survive in {table}");
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn deleted_sessions_pending_capacity_counts_current_ids_and_failed_retries() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    fill_pending(&config, MAX_PENDING_RECEIPTS);

    prepare_delete(&config, "retry", &[record("thread-000", "new-event", 7)]).unwrap();
    let receipts = pending_deletion_receipts(&config).unwrap();
    assert_eq!(receipts.len(), MAX_PENDING_RECEIPTS);
    assert!(
        receipts.iter().any(|receipt| {
            receipt.thread_id == "thread-000" && receipt.operation_id == "retry"
        })
    );
    set_deletion_outcome(&config, "retry", "thread-000", DeletionStatus::Failed, at()).unwrap();
    prepare_delete(&config, "new", &[record("extra", "event", 1)]).unwrap();
    let error =
        prepare_delete(&config, "failed-retry", &[record("thread-000", "later", 3)]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unresolved deletion member budget")
    );
    let saved = load_deleted_sessions(&config).unwrap();
    let failed = saved
        .iter()
        .find(|record| record.thread_id == "thread-000")
        .unwrap();
    assert_eq!(failed.status, DeletionStatus::Failed);
    assert_eq!(failed.token_usage().total_tokens, 8);
    assert_eq!(
        pending_deletion_receipts(&config).unwrap().len(),
        MAX_PENDING_RECEIPTS
    );
}

#[test]
fn deleted_sessions_pending_capacity_serializes_competing_operations() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    fill_pending(&config, MAX_PENDING_RECEIPTS - 1);
    let barrier = Arc::new(Barrier::new(3));
    let workers = ["first", "second"].map(|id| {
        let config = config.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            prepare_delete(&config, id, &[record(id, "event", 1)])
                .map_err(|error| error.to_string())
        })
    });
    barrier.wait();
    let results = workers.map(|worker| worker.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert!(
        error.contains("unresolved deletion member budget"),
        "{error}"
    );
    assert_eq!(
        pending_deletion_receipts(&config).unwrap().len(),
        MAX_PENDING_RECEIPTS
    );
    let saved = load_deleted_sessions(&config).unwrap();
    assert_eq!(
        saved
            .iter()
            .filter(|record| matches!(record.thread_id.as_str(), "first" | "second"))
            .count(),
        1,
        "only one competing deletion may consume the final receipt slot"
    );
}

fn ledger_bytes(config: &CollectConfig) -> Vec<u8> {
    let (_, directory, _) = location(config).unwrap();
    fs::read(directory.join(LAYOUT.data_file_name)).unwrap()
}

fn payload_sizes(config: &CollectConfig) -> (usize, usize) {
    let (total, largest): (i64, i64) = with_database(config, false, |connection| {
        Ok(connection.query_row(
            "SELECT sum(length(payload)),max(length(payload)) FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    })
    .unwrap()
    .unwrap();
    (
        usize::try_from(total).unwrap(),
        usize::try_from(largest).unwrap(),
    )
}

#[test]
fn deleted_sessions_ledger_capacity_rejects_new_and_merged_total_byte_growth() {
    for merged in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(
            &config,
            "original",
            &[record("first", "event", 1), record("other", "event", 1)],
        )
        .unwrap();
        let (total_bytes, _) = payload_sizes(&config);
        let before = ledger_bytes(&config);
        let incoming = record(if merged { "first" } else { "extra" }, "new-event", 3);
        assert!(serde_json::to_vec(&incoming).unwrap().len() < total_bytes);
        let budget = LedgerReadBudget {
            total_bytes,
            ..LEDGER_READ_BUDGET
        };

        let error = prepare_delete_with_budget(&config, "growth", &[incoming], budget).unwrap_err();
        assert!(error.to_string().contains("total-byte read budget"));
        assert_eq!(ledger_bytes(&config), before, "merged={merged}");
        assert_eq!(pending_deletion_receipts(&config).unwrap().len(), 2);
        assert_eq!(load_deleted_sessions(&config).unwrap().len(), 2);
    }
}

#[test]
fn deleted_sessions_ledger_capacity_counts_completed_records_and_accepts_the_exact_limit() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    let first = record("first", "event", 1);
    let bytes = serde_json::to_vec(&first).unwrap().len();
    prepare_delete_with_budget(
        &config,
        "original",
        &[first],
        LedgerReadBudget {
            records: 1,
            record_bytes: bytes,
            total_bytes: bytes,
        },
    )
    .unwrap();
    set_deletion_outcome(&config, "original", "first", DeletionStatus::Deleted, at()).unwrap();
    assert!(pending_deletion_receipts(&config).unwrap().is_empty());
    let before = ledger_bytes(&config);

    let error = prepare_delete_with_budget(
        &config,
        "overflow",
        &[record("extra", "event", 1)],
        LedgerReadBudget {
            records: 1,
            ..LEDGER_READ_BUDGET
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("record-count read budget"));
    assert_eq!(ledger_bytes(&config), before);
    assert!(pending_deletion_receipts(&config).unwrap().is_empty());
    assert_eq!(load_deleted_sessions(&config).unwrap().len(), 1);
}

#[test]
fn deleted_sessions_ledger_capacity_rejects_merged_single_record_growth() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory, "home");
    prepare_delete(&config, "original", &[record("first", "event", 1)]).unwrap();
    let (_, record_bytes) = payload_sizes(&config);
    let before = ledger_bytes(&config);
    let error = prepare_delete_with_budget(
        &config,
        "merge",
        &[record("first", "later", 3)],
        LedgerReadBudget {
            record_bytes,
            ..LEDGER_READ_BUDGET
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("per-record read budget"));
    assert_eq!(ledger_bytes(&config), before);
    assert_eq!(
        load_deleted_sessions(&config).unwrap()[0]
            .token_usage()
            .total_tokens,
        1
    );
}

#[test]
fn deleted_sessions_ledger_capacity_preserves_outcomes_that_would_exceed_read_limits() {
    for per_record in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let config = config(&directory, "home");
        prepare_delete(
            &config,
            "original",
            &[record("first", "event", 1), record("other", "event", 1)],
        )
        .unwrap();
        let (total_bytes, record_bytes) = payload_sizes(&config);
        let before = ledger_bytes(&config);
        let budget = if per_record {
            LedgerReadBudget {
                record_bytes,
                ..LEDGER_READ_BUDGET
            }
        } else {
            LedgerReadBudget {
                total_bytes,
                ..LEDGER_READ_BUDGET
            }
        };
        let error = set_deletion_outcome_with_budget(
            &config,
            "original",
            "first",
            DeletionStatus::Deleted,
            at(),
            budget,
        )
        .unwrap_err();
        let expected = if per_record {
            "per-record read budget"
        } else {
            "total-byte read budget"
        };
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(ledger_bytes(&config), before);
        assert_eq!(pending_deletion_receipts(&config).unwrap().len(), 2);
        assert!(
            load_deleted_sessions(&config)
                .unwrap()
                .iter()
                .all(|record| record.status == DeletionStatus::Pending)
        );

        set_deletion_outcome_with_budget(
            &config,
            "original",
            "first",
            DeletionStatus::Failed,
            at(),
            budget,
        )
        .unwrap();
        assert_eq!(pending_deletion_receipts(&config).unwrap().len(), 1);
    }
}
