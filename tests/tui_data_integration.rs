use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
test "$2" = "--stdio" || exit 42
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
        Some(2),
        "mock usage RPC deliberately makes the result partial: {}",
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
            .any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("usage disabled in fixture")))
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
