//! Opt-in synthetic Windows/Unix PTY measurements. Never part of ordinary tests.
//! The Python driver supplies verified binaries and isolated, marked fixtures.
#![cfg(any(unix, windows))]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use codex_usage_monit::domain::Provenance;
use codex_usage_monit::history::{HistoryObservation, QuotaPoint};
use codex_usage_monit::history_profile_lease::{
    TryHistoryProfileLease, try_acquire_history_profile_lease,
};
use codex_usage_monit::history_runtime::HistoryRuntime;
use codex_usage_monit::project_mapping::ProjectMappingStore;
use codex_usage_monit::source_history::LocalObservationMode;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde_json::{Value, json};

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct PtyCleanup {
    child: ChildGuard,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PtyCleanup {
    fn drop(&mut self) {
        if !matches!(self.child.0.try_wait(), Ok(Some(_))) {
            let _ = self.child.0.kill();
            let _ = self.child.0.wait();
        }
        drop(self.master.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn selected_tab(screen: &vt100::Screen, label: &str) -> bool {
    let row = screen.rows(0, screen.size().1).next().unwrap_or_default();
    row.find(label)
        .is_some_and(|column| screen.cell(0, column as u16).is_some_and(vt100::Cell::bold))
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn mutate(root: &Path, home: &Path, policy: bool) {
    let runtime = HistoryRuntime::new_with_project_mapping_store(
        root.join("state/history-v1"),
        home,
        false,
        ProjectMappingStore::new(root.join("config/project-mappings.json")),
    )
    .unwrap();
    let _lease = match try_acquire_history_profile_lease(
        runtime.state_root(),
        runtime.profile_id().clone(),
        runtime.redaction_profile(),
    )
    .unwrap()
    {
        TryHistoryProfileLease::Acquired(lease) => lease,
        TryHistoryProfileLease::Busy { .. } => panic!("synthetic profile unexpectedly busy"),
    };
    if policy {
        let active = runtime.ownership().load_manifest().unwrap();
        let codex_usage_monit::history_ownership::OwnershipManifestStatus::Initialized(active) =
            active
        else {
            panic!("uninitialized fixture")
        };
        let lease = runtime.ownership().acquire_writer_lease().unwrap();
        let authority = runtime
            .ownership()
            .authorize_v2_write(&lease, &active)
            .unwrap();
        let writer = runtime.source_history().writer(&authority).unwrap();
        writer
            .update_source_metadata(
                &"node-00000000000000000000000000000001".parse().unwrap(),
                |source| {
                    source.set_quota_matches_local_account(false);
                    Ok(())
                },
            )
            .unwrap();
    } else {
        let base = DateTime::parse_from_rfc3339(&std::env::var("N2_OBSERVED_AT").unwrap())
            .unwrap()
            .with_timezone(&Utc);
        runtime
            .record_local_observation(
                &HistoryObservation {
                    observed_at: base + chrono::Duration::minutes(5),
                    quota_points: vec![QuotaPoint {
                        observed_at: base + chrono::Duration::minutes(5),
                        limit_id: "codex".to_owned(),
                        duration_mins: 10_080,
                        resets_at: base + chrono::Duration::days(6),
                        used_percent: 37.0,
                        remaining_percent: 63.0,
                        provenance: Provenance::ServerSnapshot,
                    }],
                    ..HistoryObservation::default()
                },
                LocalObservationMode::Incremental,
            )
            .unwrap();
    }
}

#[test]
#[ignore = "controlled synthetic measurement; run scripts/windows/measure-tui-history.py"]
fn synthetic_history_sample() {
    let root = PathBuf::from(std::env::var_os("N2_FIXTURE_ROOT").expect("N2_FIXTURE_ROOT"));
    let home = PathBuf::from(std::env::var_os("N2_CODEX_HOME").expect("N2_CODEX_HOME"));
    let binary = PathBuf::from(std::env::var_os("N2_BINARY").expect("N2_BINARY"));
    let capture = PathBuf::from(std::env::var_os("N2_CAPTURE_DIR").expect("N2_CAPTURE_DIR"));
    assert!(
        root.is_absolute() && home.is_absolute() && binary.is_absolute() && capture.is_absolute()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("synthetic-measurement.txt")).unwrap(),
        "codex-usage-monit synthetic measurement v1\n"
    );
    let steady = std::env::var("N2_MODE").as_deref() == Ok("steady");
    std::fs::create_dir_all(&capture).unwrap();
    let trace = capture.join("trace.jsonl");
    let events = capture.join("events.jsonl");
    let mut command = CommandBuilder::new(&binary);
    command.args([
        "--codex-home",
        home.to_str().unwrap(),
        "--days",
        "3650",
        "--offline",
        "--no-rollout-cache",
        "--trace-log",
        trace.to_str().unwrap(),
        "--startup-log",
        capture.join("startup.jsonl").to_str().unwrap(),
        "--log-file",
        events.to_str().unwrap(),
        "--log-level",
        "debug",
    ]);
    command.env("CODEX_USAGE_MONIT_STATE_DIR", root.join("state"));
    command.env("CODEX_USAGE_MONIT_CONFIG_DIR", root.join("config"));
    command.env("CODEX_USAGE_MONIT_CACHE_DIR", root.join("cache"));
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env_remove("NO_COLOR");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let started = Instant::now();
    let child = ChildGuard(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    println!("N2_PID={}", child.0.process_id().unwrap());
    std::io::stdout().flush().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, output) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(size) = reader.read(&mut buffer) {
            if size == 0 || sender.send(buffer[..size].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut session = PtyCleanup {
        child,
        master: Some(pair.master),
        reader: Some(reader_thread),
    };
    let mut parser = vt100::Parser::new(24, 120, 0);
    let query = b"\x1b[6n";
    let mut query_position = 0;
    let mut phases = Vec::new();
    let mut phase = "initial";
    let mut phase_started = Instant::now();
    let mut phase_started_at = Utc::now();
    let mut initial_overview_requested = false;
    let mut initial_ready = None;
    let mut timeout = false;
    let mut disconnected = false;
    let mut raw = Vec::new();
    loop {
        match output.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                raw.extend_from_slice(&bytes);
                for byte in bytes {
                    parser.process(&[byte]);
                    if byte == query[query_position] {
                        query_position += 1;
                    } else {
                        query_position = usize::from(byte == query[0]);
                    }
                    if query_position == query.len() {
                        query_position = 0;
                        let (row, column) = parser.screen().cursor_position();
                        // ConPTY needs one complete startup cursor reply.
                        let reply = format!("\x1b[{};{}R", row + 1, column + 1);
                        writer.write_all(reply.as_bytes()).unwrap();
                        writer.flush().unwrap();
                        println!("N2_CURSOR_REPLY={}{}", row + 1, column + 1);
                        std::io::stdout().flush().unwrap();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                disconnected = true;
                break;
            }
        }
        // Windows diagnostic logs are exclusively byte-range locked while the
        // product runs. Observe the screen now and validate phases from the
        // unlocked trace after exit, rather than silently failing live reads.
        if steady
            && phase == "initial"
            && !initial_overview_requested
            && selected_tab(parser.screen(), "Summary")
        {
            writer.write_all(b"1").unwrap();
            writer.flush().unwrap();
            initial_overview_requested = true;
        }
        let ready = if phase == "initial" {
            parser
                .screen()
                .contents()
                .contains("codex-usage-monit | desktop")
        } else {
            phase_started.elapsed() >= Duration::from_secs(45)
        };
        let expected_tab = if phase == "initial" {
            "Overview"
        } else if phase == "idle_cache" || phase == "quota_policy_changed" {
            "Trends"
        } else {
            "Summary"
        };
        let ready = ready && selected_tab(parser.screen(), expected_tab);
        if ready {
            let elapsed = started.elapsed().as_secs_f64();
            phases.push(json!({"phase": phase, "elapsedSeconds": elapsed, "phaseSeconds": phase_started.elapsed().as_secs_f64(),
                "startedAt": phase_started_at, "finishedAt": Utc::now(), "selectedTab": expected_tab,
                "screen": parser.screen().contents()}));
            if phase == "initial" {
                initial_ready = Some(elapsed);
                if !steady {
                    break;
                }
                writer.write_all(b"2").unwrap();
                writer.flush().unwrap();
                phase = "idle_cache";
            } else if phase == "idle_cache" {
                writer.write_all(b"u").unwrap();
                writer.flush().unwrap();
                mutate(&root, &home, false);
                phase = "quota_changed";
            } else if phase == "quota_changed" {
                writer.write_all(b"2").unwrap();
                writer.flush().unwrap();
                mutate(&root, &home, true);
                phase = "quota_policy_changed";
            } else {
                break;
            }
            phase_started = Instant::now();
            phase_started_at = Utc::now();
        }
        let deadline = if phase == "initial" { 30 } else { 55 };
        if phase_started.elapsed() > Duration::from_secs(deadline) {
            timeout = true;
            break;
        }
    }
    let screen = parser.screen().contents();
    let _ = writer.write_all(b"q");
    let _ = writer.flush();
    let exit_started = Instant::now();
    let exit = loop {
        if let Some(status) = session.child.0.try_wait().unwrap() {
            break Some(status.success());
        }
        if exit_started.elapsed() > Duration::from_secs(5) {
            let _ = session.child.0.kill();
            let _ = session.child.0.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    std::fs::write(capture.join("terminal.txt"), screen).unwrap();
    std::fs::write(capture.join("terminal.raw"), raw).unwrap();
    drop(writer);
    drop(session);
    let traces = records(&trace);
    let event_records = records(&events);
    for phase in &mut phases {
        let start = DateTime::parse_from_rfc3339(phase["startedAt"].as_str().unwrap()).unwrap();
        let end = DateTime::parse_from_rfc3339(phase["finishedAt"].as_str().unwrap()).unwrap();
        let at = |event: &Value| {
            event["at"]
                .as_str()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        };
        phase["traceRecords"] = json!(
            traces
                .iter()
                .take_while(|event| at(event).is_none_or(|time| time <= end))
                .count()
        );
        phase["refreshesInPhase"] = json!(
            event_records
                .iter()
                .filter(|event| event["event"] == "tui.refresh"
                    && at(event).is_some_and(|time| time >= start && time <= end))
                .count()
        );
    }
    let initial_trace_confirmed = phases.first().is_some_and(|phase| {
        let end = DateTime::parse_from_rfc3339(phase["finishedAt"].as_str().unwrap()).unwrap();
        traces.iter().any(|event| {
            event["event"] == "span_finish"
                && event["stage"] == "tui.initial_data_ready"
                && event["at"]
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|at| at <= end)
        })
    });
    let steady_refreshes_confirmed = phases.iter().skip(1).all(|phase| {
        phase["refreshesInPhase"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    });
    let v2 = traces.iter().any(|event| {
        event["stage"] == "history.stage_load"
            && event["fields"]["writeBackend"] == "v2"
            && event["fields"]["readOnly"] == false
    });
    let expected_phases: &[&str] = if steady {
        &[
            "initial",
            "idle_cache",
            "quota_changed",
            "quota_policy_changed",
        ]
    } else {
        &["initial"]
    };
    let completed = phases
        .iter()
        .filter_map(|value| value["phase"].as_str())
        .eq(expected_phases.iter().copied());
    let failure_reason = if disconnected && !completed {
        Some("PTY disconnected before all planned phases completed")
    } else if timeout {
        Some("diagnostic phase deadline exceeded")
    } else if !completed {
        Some("planned phase sequence incomplete")
    } else {
        None
    };
    let report = json!({"phases": phases, "phasesComplete": completed, "failureReason": failure_reason,
        "initialReadySeconds": initial_ready, "initialTraceConfirmed": initial_trace_confirmed,
        "steadyRefreshesConfirmed": steady_refreshes_confirmed, "steadyObservationSeconds": 45,
        "exceededFormalEightSeconds": initial_ready.map(|seconds| seconds >= 8.0),
        "initialDiagnosticTimeoutSeconds": 30, "steadyDiagnosticTimeoutSeconds": 55,
        "timedOut": timeout, "v2Confirmed": v2, "successfulExit": exit,
        "wallSeconds": started.elapsed().as_secs_f64(), "mode": if steady { "steady" } else { "startup" }});
    std::fs::write(
        capture.join("sample.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert!(v2, "excluded sample: source-aware V2 was not confirmed");
    if steady {
        assert!(
            traces
                .iter()
                .any(|event| event["stage"] == "history.v2.query"
                    && event["fields"]["sourceScope"] == "local")
        );
        assert!(
            traces
                .iter()
                .any(|event| event["stage"] == "history.v2.query"
                    && event["fields"]["sourceScope"] == "all")
        );
    }
    assert!(
        completed
            && initial_trace_confirmed
            && steady_refreshes_confirmed
            && !timeout
            && initial_ready.is_some()
            && exit == Some(true),
        "measurement did not complete: {report}"
    );
}
