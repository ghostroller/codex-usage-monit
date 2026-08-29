use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{Duration, Utc};
use serde_json::Value;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codex-usage-monit")
}

fn fixture_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("codex-home")
        .join("normal")
}

fn isolated_command(temp: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .env("CODEX_USAGE_MONIT_STATE_DIR", temp.join("state"))
        .env("CODEX_USAGE_MONIT_CONFIG_DIR", temp.join("config"))
        .env("CODEX_USAGE_MONIT_CACHE_DIR", temp.join("cache"));
    command
}

fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON output: {error}\nstatus={:?}\nstdout={}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn write_current_rollout_fixture(codex_home: &Path) {
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let started_at = Utc::now() - Duration::minutes(2);
    let completed_at = started_at + Duration::seconds(5);
    let thread_id = "019f52ac-7a9f-7fd1-8dda-e775ef950786";
    let turn_id = "turn-cli-report-integration";
    let records = [
        serde_json::json!({
            "timestamp": started_at,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "timestamp": started_at,
                "cwd": "/work/cli-report-project",
                "originator": "codex_cli_rs"
            }
        }),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(1),
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "started_at": started_at + Duration::seconds(1)
            }
        }),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(2),
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "CLI report integration task"}
        }),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(3),
            "type": "turn_context",
            "payload": {"turn_id": turn_id, "model": "gpt-5.3-codex", "effort": "high"}
        }),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(4),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 1200,
                    "cached_input_tokens": 200,
                    "output_tokens": 300,
                    "reasoning_output_tokens": 100,
                    "total_tokens": 1500
                }}
            }
        }),
        serde_json::json!({
            "timestamp": completed_at,
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": turn_id}
        }),
    ];
    let contents = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(sessions.join("rollout-cli-reports.jsonl"), contents).unwrap();
    std::fs::write(
        codex_home.join("session_index.jsonl"),
        serde_json::to_string(&serde_json::json!({
            "id": thread_id,
            "thread_name": "CLI report integration task",
            "updated_at": completed_at
        }))
        .unwrap()
            + "\n",
    )
    .unwrap();
}

#[test]
fn real_binary_collects_the_fixture_codex_home_without_user_data() {
    let temp = tempfile::tempdir().unwrap();
    let output = isolated_command(temp.path())
        .args([
            "--codex-home",
            fixture_home().to_str().unwrap(),
            "--days",
            "3650",
            "--offline",
            "--no-rollout-cache",
            "snapshot",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "offline fixture is intentionally partial: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = parse_json(&output);
    assert_eq!(
        snapshot["tasks"][0]["title"],
        "Fixture-driven integration task"
    );
    assert_eq!(
        snapshot["tasks"][0]["threadId"],
        "019f52ac-7a9f-7fd1-8dda-e775ef950785"
    );
    assert_eq!(snapshot["turns"][0]["model"], "gpt-5.3-codex");
    assert!(
        snapshot["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["source"] == "rollout_jsonl")
    );
}

#[test]
fn real_binary_exposes_summary_trends_and_unified_health_json() {
    let temp = tempfile::tempdir().unwrap();
    let codex_home = temp.path().join("codex-home");
    let history_dir = temp.path().join("history");
    write_current_rollout_fixture(&codex_home);

    let common = [
        "--codex-home",
        codex_home.to_str().unwrap(),
        "--days",
        "1",
        "--offline",
        "--no-rollout-cache",
    ];
    let summary = isolated_command(temp.path())
        .args(common)
        .args([
            "summary",
            "--range",
            "7d",
            "--grain",
            "1h",
            "--metric",
            "estimated",
            "--long-context",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            history_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(summary.status.code(), Some(2));
    let summary = parse_json(&summary);
    assert_eq!(summary["schemaVersion"], 1);
    assert_eq!(summary["range"], "7d");
    assert_eq!(summary["grain"], "1h");
    assert_eq!(summary["metric"], "estimated");
    assert_eq!(summary["apiLongContext"], true);
    assert_eq!(summary["projects"][0]["label"], "cli-report-project");
    assert_eq!(
        summary["projects"][0]["sessions"][0]["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let trends = isolated_command(temp.path())
        .args(common)
        .args([
            "trends",
            "--long-context",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            history_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(trends.status.code(), Some(2));
    let trends = parse_json(&trends);
    assert_eq!(trends["schemaVersion"], 1);
    assert_eq!(trends["apiLongContextMultiplier"], true);
    assert!(!trends["fifteenMinuteTokens"].as_array().unwrap().is_empty());
    assert!(trends.get("halfHourTokens").is_none());

    let health = isolated_command(temp.path())
        .args(common)
        .args([
            "health",
            "--format",
            "json",
            "--compact",
            "--history-dir",
            history_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(health.status.code(), Some(2));
    let health = parse_json(&health);
    assert_eq!(health["schemaVersion"], 1);
    assert!(health.get("snapshot").is_some());
    assert!(health.get("history").is_some());
    assert!(health.get("recorder").is_some());
    assert!(health.get("service").is_some() || health.get("serviceError").is_some());
    assert!(health["snapshot"].get("codexHome").is_none());
}

#[cfg(unix)]
#[test]
fn real_binary_combines_fixture_rollouts_with_a_mock_codex_app_server() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let codex = bin.join("codex");
    fs::write(
        &codex,
        r#"#!/bin/sh
test "$1" = "app-server" || exit 41
test -z "$2" || exit 42
IFS= read -r initialize || exit 43
printf '%s\n' '{"id":1,"result":{"userAgent":"fixture-app-server"}}'
IFS= read -r initialized || exit 44
IFS= read -r limits || exit 45
IFS= read -r usage || exit 46
printf '%s\n' '{"id":3,"error":{"code":-32601,"message":"usage disabled in fixture"}}'
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","limitName":"Codex","planType":"plus","primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1783834200},"secondary":{"usedPercent":27,"windowDurationMins":10080,"resetsAt":1784439000}},"rateLimitsByLimitId":null,"rateLimitResetCredits":{"availableCount":1,"credits":[]}}}'
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&codex, permissions).unwrap();

    let mut path = std::ffi::OsString::from(&bin);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(":");
        path.push(existing);
    }
    let perf_log = temp.path().join("perf.jsonl");
    let output = isolated_command(temp.path())
        .env("PATH", path)
        .args([
            "--codex-home",
            fixture_home().to_str().unwrap(),
            "--days",
            "3650",
            "--no-rollout-cache",
            "--perf-log",
            perf_log.to_str().unwrap(),
            "snapshot",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "an unavailable optional usage RPC must not make the snapshot partial: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = parse_json(&output);
    assert_eq!(snapshot["limits"][0]["limitId"], "codex");
    assert_eq!(snapshot["limits"][0]["primary"]["usedPercent"], 42.0);
    assert_eq!(
        snapshot["tasks"][0]["title"],
        "Fixture-driven integration task"
    );
    assert!(
        snapshot["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| {
                warning
                    .as_str()
                    .is_none_or(|warning| !warning.contains("usage disabled in fixture"))
            })
    );

    let refresh = fs::read_to_string(perf_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["event"] == "refresh")
        .expect("online fixture collection must emit a refresh performance event");
    assert!(
        refresh["metrics"]["stages"]["accountUs"]
            .as_u64()
            .is_some_and(|account_us| account_us > 0),
        "parallel App Server collection must retain its elapsed time: {refresh}"
    );
}
