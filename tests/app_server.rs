use std::time::Duration;

#[cfg(any(unix, windows))]
use std::ffi::OsString;
#[cfg(any(unix, windows))]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(unix, windows))]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::Instant;

use chrono::{TimeZone, Utc};
use codex_usage_monit::app_server::{
    fetch_account_snapshot, parse_account_usage_result, parse_rate_limit_reset_credits_result,
    parse_rate_limits_result,
};
use codex_usage_monit::config::CollectConfig;
use codex_usage_monit::domain::{Provenance, RateLimitResetCreditsSnapshot};
#[cfg(unix)]
use codex_usage_monit::rollout::RolloutCache;
#[cfg(unix)]
use codex_usage_monit::snapshot::collect_snapshot_cached;
use pretty_assertions::assert_eq;
use serde_json::json;

#[cfg(any(unix, windows))]
static PATH_LOCK: Mutex<()> = Mutex::new(());

#[cfg(any(unix, windows))]
struct PathRestore(Option<OsString>);

#[cfg(any(unix, windows))]
impl Drop for PathRestore {
    fn drop(&mut self) {
        // SAFETY: PATH mutations in this test module are serialized by PATH_LOCK.
        unsafe {
            match &self.0 {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

#[cfg(unix)]
fn with_mock_codex<T>(script: &str, run: impl FnOnce(&std::path::Path) -> T) -> T {
    let _lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("codex");
    fs::write(&executable, script).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();

    let old_path = std::env::var_os("PATH");
    let mut path = OsString::from(directory.path());
    if let Some(old_path) = &old_path {
        path.push(":");
        path.push(old_path);
    }
    // SAFETY: PATH mutations in this test module are serialized by PATH_LOCK.
    unsafe { std::env::set_var("PATH", path) };
    let _restore = PathRestore(old_path);

    run(directory.path())
}

#[cfg(windows)]
fn with_mock_codex<T>(script: &str, run: impl FnOnce(&std::path::Path) -> T) -> T {
    let _lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let directory = tempfile::tempdir().unwrap();
    // npm's Windows cmd-shim creates all three siblings. The bare one is a
    // POSIX shell script and must not shadow the executable `.cmd` shim.
    fs::write(directory.path().join("codex"), "#!/bin/sh\nexit 91\n").unwrap();
    let executable = directory.path().join("codex.cmd");
    fs::write(&executable, script).unwrap();

    let old_path = std::env::var_os("PATH");
    let path = std::env::join_paths(
        std::iter::once(directory.path().to_path_buf()).chain(
            old_path
                .as_deref()
                .into_iter()
                .flat_map(std::env::split_paths),
        ),
    )
    .unwrap();
    // SAFETY: PATH mutations in this test module are serialized by PATH_LOCK.
    unsafe { std::env::set_var("PATH", path) };
    let _restore = PathRestore(old_path);

    run(directory.path())
}

#[test]
fn parses_multi_bucket_rate_limits_and_prefers_the_keyed_view() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let result = json!({
        "rateLimits": {
            "limitId": "legacy",
            "primary": { "usedPercent": 99, "windowDurationMins": 300, "resetsAt": null }
        },
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "limitName": "Codex",
                "planType": "plus",
                "primary": {
                    "usedPercent": 37.5,
                    "windowDurationMins": 300,
                    "resetsAt": 1783834200
                },
                "secondary": {
                    "usedPercent": 12,
                    "windowDurationMins": 10080,
                    "resetsAt": 1784439000
                },
                "credits": {
                    "hasCredits": true,
                    "unlimited": false,
                    "balance": "18.25"
                },
                "rateLimitReachedType": null,
                "futureField": { "isIgnored": true }
            },
            "reviews": {
                "limitId": null,
                "limitName": "Reviews",
                "primary": null,
                "secondary": null,
                "credits": null
            }
        }
    });

    let limits = parse_rate_limits_result(&result, as_of).unwrap();

    assert_eq!(limits.len(), 2);
    assert_eq!(limits[0].limit_id, "codex");
    assert_eq!(limits[0].limit_name.as_deref(), Some("Codex"));
    assert_eq!(limits[0].provenance, Provenance::ServerSnapshot);
    assert_eq!(limits[0].as_of, as_of);
    let primary = limits[0].primary.as_ref().unwrap();
    assert_eq!(primary.used_percent, 37.5);
    assert_eq!(primary.remaining_percent, 62.5);
    assert_eq!(primary.window_duration_mins, Some(300));
    assert_eq!(primary.resets_at.unwrap().timestamp(), 1_783_834_200);
    assert_eq!(limits[0].secondary.as_ref().unwrap().label(), "week");
    assert_eq!(
        limits[0].credits.as_ref().unwrap().balance.as_deref(),
        Some("18.25")
    );
    assert_eq!(limits[1].limit_id, "reviews");
}

#[test]
fn parses_legacy_rate_limit_and_millisecond_reset_timestamp() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let response = json!({
        "id": 2,
        "result": {
            "rateLimits": {
                "limitId": null,
                "primary": {
                    "usedPercent": "100.5",
                    "windowDurationMins": "300",
                    "resetsAt": 1783834200123_i64
                }
            },
            "rateLimitsByLimitId": null
        }
    });

    let limits = parse_rate_limits_result(&response, as_of).unwrap();

    assert_eq!(limits[0].limit_id, "default");
    assert_eq!(limits[0].primary.as_ref().unwrap().remaining_percent, 0.0);
    assert_eq!(
        limits[0]
            .primary
            .as_ref()
            .unwrap()
            .resets_at
            .unwrap()
            .timestamp_millis(),
        1_783_834_200_123
    );
}

#[test]
fn parses_rate_limit_reset_credits_with_provenance_and_timestamp() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let credits = parse_rate_limit_reset_credits_result(
        &json!({
            "id": 2,
            "result": {
                "rateLimitResetCredits": {
                    "availableCount": 3,
                    "credits": [
                        {
                            "id": "opaque-sensitive-id",
                            "grantedAt": 1783834200,
                            "expiresAt": 1784439000,
                            "status": "available",
                            "resetType": "codexRateLimits",
                            "title": "Reset Codex limits",
                            "description": "One reset opportunity"
                        },
                        {
                            "id": "another-sensitive-id",
                            "grantedAt": 1783834300,
                            "expiresAt": null,
                            "status": "scheduled_by_future_server",
                            "resetType": "futureResetType",
                            "title": null,
                            "description": null
                        }
                    ]
                }
            }
        }),
        as_of,
    )
    .unwrap()
    .unwrap();

    assert_eq!(credits.available_count, 3);
    assert_eq!(credits.provenance, Provenance::ServerSnapshot);
    assert_eq!(credits.as_of, as_of);
    assert!(credits.details_are_truncated());
    let details = credits.credits.as_ref().unwrap();
    assert_eq!(details.len(), 2);
    assert_eq!(details[0].granted_at.timestamp(), 1_783_834_200);
    assert_eq!(details[0].expires_at.unwrap().timestamp(), 1_784_439_000);
    assert_eq!(details[0].status, "available");
    assert_eq!(details[0].reset_type, "codexRateLimits");
    assert_eq!(details[0].title.as_deref(), Some("Reset Codex limits"));
    assert_eq!(
        details[0].description.as_deref(),
        Some("One reset opportunity")
    );
    assert_eq!(details[1].expires_at, None);
    assert_eq!(details[1].status, "scheduled_by_future_server");
    assert_eq!(details[1].reset_type, "futureResetType");

    let serialized = serde_json::to_value(&credits).unwrap();
    assert!(serialized["credits"][0].get("id").is_none());
    assert!(!serialized.to_string().contains("opaque-sensitive-id"));

    for result in [json!({}), json!({ "rateLimitResetCredits": null })] {
        assert_eq!(
            parse_rate_limit_reset_credits_result(&result, as_of).unwrap(),
            None
        );
    }
}

#[test]
fn distinguishes_unavailable_empty_and_truncated_reset_credit_details() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let unavailable = parse_rate_limit_reset_credits_result(
        &json!({
            "rateLimitResetCredits": { "availableCount": 2, "credits": null }
        }),
        as_of,
    )
    .unwrap()
    .unwrap();
    assert_eq!(unavailable.credits, None);
    assert!(!unavailable.details_are_truncated());
    assert_eq!(
        serde_json::to_value(&unavailable).unwrap()["credits"],
        json!(null)
    );

    let empty = parse_rate_limit_reset_credits_result(
        &json!({
            "rateLimitResetCredits": { "availableCount": 0, "credits": [] }
        }),
        as_of,
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty.credits, Some(Vec::new()));
    assert!(!empty.details_are_truncated());
    assert_eq!(serde_json::to_value(&empty).unwrap()["credits"], json!([]));

    let omitted = parse_rate_limit_reset_credits_result(
        &json!({
            "rateLimitResetCredits": { "availableCount": 1 }
        }),
        as_of,
    )
    .unwrap()
    .unwrap();
    assert_eq!(omitted.credits, None);

    let legacy: RateLimitResetCreditsSnapshot = serde_json::from_value(json!({
        "availableCount": 1,
        "provenance": "server_snapshot",
        "asOf": as_of,
    }))
    .unwrap();
    assert_eq!(legacy.credits, None);
}

#[test]
fn rejects_malformed_rate_limit_reset_credit_details_with_field_context() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let malformed = [
        (
            json!(true),
            "rateLimitResetCredits.credits must be an array or null",
        ),
        (json!([null]), "rateLimitResetCredits.credits[0]"),
        (
            json!([{
                "id": "opaque",
                "grantedAt": null,
                "status": "available",
                "resetType": "codexRateLimits"
            }]),
            "rateLimitResetCredits.credits[0].grantedAt",
        ),
        (
            json!([{
                "id": "opaque",
                "grantedAt": 1783834200,
                "expiresAt": "not-a-time",
                "status": "available",
                "resetType": "codexRateLimits"
            }]),
            "rateLimitResetCredits.credits[0].expiresAt",
        ),
        (
            json!([{
                "id": "opaque",
                "grantedAt": 1783834200,
                "status": 42,
                "resetType": "codexRateLimits"
            }]),
            "rateLimitResetCredits.credits[0].status",
        ),
        (
            json!([{
                "id": "opaque",
                "grantedAt": 1783834200,
                "status": "available",
                "resetType": {}
            }]),
            "rateLimitResetCredits.credits[0].resetType",
        ),
    ];

    for (credits, expected) in malformed {
        let error = parse_rate_limit_reset_credits_result(
            &json!({
                "rateLimitResetCredits": {
                    "availableCount": 1,
                    "credits": credits
                }
            }),
            as_of,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[test]
fn parses_reset_credit_timestamps_as_strict_unix_seconds() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let credits = parse_rate_limit_reset_credits_result(
        &json!({
            "rateLimitResetCredits": {
                "availableCount": 1,
                "credits": [{
                    "id": "opaque",
                    "grantedAt": 1_783_834_200_123_i64,
                    "expiresAt": null,
                    "status": "available",
                    "resetType": "codexRateLimits"
                }]
            }
        }),
        as_of,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        credits.credits.as_ref().unwrap()[0].granted_at.timestamp(),
        1_783_834_200_123
    );

    for (timestamp, expected) in [
        (
            json!("1783834200"),
            "rateLimitResetCredits.credits[0].grantedAt",
        ),
        (
            json!(i64::MAX),
            "rateLimitResetCredits.credits[0].grantedAt",
        ),
        (
            json!(9_223_372_036_854_775_808_u64),
            "rateLimitResetCredits.credits[0].grantedAt",
        ),
    ] {
        let error = parse_rate_limit_reset_credits_result(
            &json!({
                "rateLimitResetCredits": {
                    "availableCount": 1,
                    "credits": [{
                        "id": "opaque",
                        "grantedAt": timestamp,
                        "status": "available",
                        "resetType": "codexRateLimits"
                    }]
                }
            }),
            as_of,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[test]
fn rejects_invalid_rate_limit_reset_credit_counts_with_field_context() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let invalid_counts = [
        None,
        Some(json!(null)),
        Some(json!(-1)),
        Some(json!(1.5)),
        Some(json!(9_223_372_036_854_775_808_u64)),
        Some(json!("18446744073709551616")),
    ];

    for available_count in invalid_counts {
        let mut reset_credits = serde_json::Map::new();
        if let Some(available_count) = available_count {
            reset_credits.insert("availableCount".to_string(), available_count);
        }
        let error = parse_rate_limit_reset_credits_result(
            &json!({
                "rateLimitResetCredits": reset_credits
            }),
            as_of,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("rateLimitResetCredits.availableCount"),
            "unexpected error: {error:#}"
        );
    }
}

#[test]
fn parses_account_usage_numbers_and_nullable_buckets() {
    let usage = parse_account_usage_result(&json!({
        "summary": {
            "lifetimeTokens": "9007199254740993",
            "peakDailyTokens": 2000,
            "longestRunningTurnSec": null,
            "currentStreakDays": 3,
            "longestStreakDays": 8
        },
        "dailyUsageBuckets": [
            { "startDate": "2026-07-11", "tokens": 1200 },
            { "startDate": "2026-07-12", "tokens": "800" }
        ]
    }))
    .unwrap();

    assert_eq!(usage.lifetime_tokens, Some(9_007_199_254_740_993));
    assert_eq!(usage.peak_daily_tokens, Some(2000));
    assert_eq!(usage.longest_running_turn_sec, None);
    assert_eq!(usage.current_streak_days, Some(3));
    assert_eq!(usage.longest_streak_days, Some(8));
    assert_eq!(usage.daily_usage_buckets.len(), 2);
    assert_eq!(usage.daily_usage_buckets[1].tokens, 800);

    let usage = parse_account_usage_result(&json!({
        "summary": {},
        "dailyUsageBuckets": null
    }))
    .unwrap();
    assert!(usage.daily_usage_buckets.is_empty());
}

#[test]
fn rejects_malformed_payloads_with_field_context() {
    let as_of = Utc.with_ymd_and_hms(2026, 7, 12, 4, 30, 0).unwrap();
    let error = parse_rate_limits_result(
        &json!({
            "rateLimits": {
                "primary": { "usedPercent": "many", "windowDurationMins": 300 }
            }
        }),
        as_of,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("primary.usedPercent"));

    let error = parse_account_usage_result(&json!({
        "summary": {},
        "dailyUsageBuckets": [{ "startDate": "2026-07-12", "tokens": -1 }]
    }))
    .unwrap_err();
    assert!(format!("{error:#}").contains("dailyUsageBuckets[0].tokens"));
}

#[test]
fn offline_mode_returns_without_starting_codex() {
    let config = CollectConfig {
        offline: true,
        app_server_timeout: Duration::ZERO,
        ..CollectConfig::default()
    };

    let snapshot = fetch_account_snapshot(&config).unwrap();

    assert!(snapshot.limits.is_empty());
    assert!(snapshot.usage.is_none());
    assert_eq!(snapshot.warnings.len(), 1);
    assert!(snapshot.warnings[0].contains("offline mode"));
}

#[cfg(windows)]
#[test]
fn fetches_limits_through_a_codex_cmd_shim() {
    let script = r#"@echo off
if not "%~1"=="app-server" exit /b 41
if not "%~2"=="" exit /b 42
set /p initialize=
echo {"id":1,"result":{"userAgent":"mock"}}
set /p account_requests=
echo {"id":3,"error":{"code":-32600,"message":"Invalid request: unknown variant `account/usage/read`, expected one of `initialize`, `account/rateLimits/read`, `thread/start`"}}
echo {"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":42,"windowDurationMins":300}},"rateLimitsByLimitId":null}}
rem Keep the shim alive while the client writes the remaining RPC messages.
rem A redirected set /p can read ahead and consume multiple LF-delimited messages.
more >nul
"#;

    with_mock_codex(script, |directory| {
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_secs(5),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();

        assert_eq!(snapshot.limits.len(), 1);
        assert_eq!(snapshot.limits[0].limit_id, "codex");
        assert_eq!(
            snapshot.limits[0].primary.as_ref().unwrap().used_percent,
            42.0
        );
        assert!(snapshot.usage.is_none());
        assert!(snapshot.warnings.is_empty());
    });
}

#[cfg(unix)]
#[test]
fn fetches_limits_and_preserves_a_nonfatal_usage_rpc_error() {
    let script = r#"#!/bin/sh
test "$1" = "app-server" || exit 41
test -z "$2" || exit 42
IFS= read -r initialize || exit 43
case "$initialize" in *'"method":"initialize"'*) ;; *) exit 44 ;; esac
printf '%s\n' '{"id":1,"result":{"userAgent":"mock"}}'
IFS= read -r initialized || exit 45
case "$initialized" in *'"method":"initialized"'*) ;; *) exit 46 ;; esac
IFS= read -r limits || exit 47
case "$limits" in *'"method":"account/rateLimits/read"'*) ;; *) exit 48 ;; esac
IFS= read -r usage || exit 49
case "$usage" in *'"method":"account/usage/read"'*) ;; *) exit 50 ;; esac
printf '%s\n' 'this is not json'
printf '%s\n' '{"id":3,"error":{"code":-32601,"message":"usage disabled"}}'
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1783834200}},"rateLimitsByLimitId":null,"rateLimitResetCredits":{"availableCount":3,"credits":[{"id":"opaque-sensitive-id","grantedAt":1783834200,"expiresAt":1784439000,"status":"available","resetType":"codexRateLimits","title":"Reset Codex limits","description":"One reset opportunity"},{"id":"bad-detail","grantedAt":"not-seconds","status":"available","resetType":"codexRateLimits"}]}}}'
"#;

    with_mock_codex(script, |directory| {
        let trace =
            codex_usage_monit::startup::StartupTrace::enabled(Instant::now(), None).unwrap();
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_secs(2),
            startup_trace: trace.clone(),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();
        trace.stop();

        assert_eq!(snapshot.limits.len(), 1);
        assert_eq!(snapshot.limits[0].limit_id, "codex");
        assert_eq!(
            snapshot.limits[0].primary.as_ref().unwrap().used_percent,
            42.0
        );
        let reset_credits = snapshot.rate_limit_reset_credits.as_ref().unwrap();
        assert_eq!(reset_credits.available_count, 3);
        assert_eq!(reset_credits.provenance, Provenance::ServerSnapshot);
        assert!(reset_credits.details_are_truncated());
        assert!(snapshot.rate_limit_reset_credits_partial);
        let credit = &reset_credits.credits.as_ref().unwrap()[0];
        assert_eq!(credit.granted_at.timestamp(), 1_783_834_200);
        assert_eq!(credit.expires_at.unwrap().timestamp(), 1_784_439_000);
        assert_eq!(credit.status, "available");
        assert_eq!(credit.reset_type, "codexRateLimits");
        assert_eq!(credit.title.as_deref(), Some("Reset Codex limits"));
        assert!(snapshot.usage.is_none());
        assert!(snapshot.errors.is_empty());
        assert!(
            !snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("account/usage/read failed"))
        );
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("malformed"))
        );
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| { warning.contains("rateLimitResetCredits.credits[1].grantedAt") })
        );
        let stages = trace
            .report()
            .events
            .into_iter()
            .map(|event| event.stage)
            .collect::<Vec<_>>();
        for expected in [
            "app_server.spawn",
            "app_server.initialize",
            "app_server.account_reads",
            "app_server.parse_responses",
            "app_server.shutdown",
            "app_server.total",
        ] {
            assert!(stages.iter().any(|stage| stage == expected));
        }
    });
}

#[cfg(unix)]
#[test]
fn snapshot_boundary_includes_fresh_app_server_timestamps() {
    let script = r#"#!/bin/sh
test "$1" = "app-server" || exit 41
IFS= read -r initialize || exit 42
printf '%s\n' '{"id":1,"result":{"userAgent":"mock"}}'
IFS= read -r initialized || exit 43
IFS= read -r limits || exit 44
IFS= read -r usage || exit 45
sleep 0.02
printf '%s\n' '{"id":3,"error":{"code":-32601,"message":"usage disabled"}}'
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1783834200}},"rateLimitsByLimitId":null,"rateLimitResetCredits":{"availableCount":0,"credits":[]}}}'
"#;

    with_mock_codex(script, |directory| {
        let codex_home = directory.join("home");
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        let config = CollectConfig {
            codex_home,
            app_server_timeout: Duration::from_secs(2),
            ..CollectConfig::default()
        };
        let mut cache = RolloutCache::new();

        let result = collect_snapshot_cached(&config, None, true, &mut cache);
        let collection_completed_at = Utc::now();

        assert_eq!(result.snapshot.limits.len(), 1);
        assert_eq!(result.snapshot.limits[0].limit_id, "codex");
        assert_eq!(result.snapshot.limits[0].as_of, result.snapshot.as_of);
        let reset_credits = result.snapshot.rate_limit_reset_credits.as_ref().unwrap();
        assert_eq!(reset_credits.available_count, 0);
        assert_eq!(reset_credits.as_of, result.snapshot.as_of);
        assert!(result.snapshot.as_of < collection_completed_at);
        assert!(
            result
                .history_observation
                .half_hour_buckets
                .iter()
                .all(|bucket| bucket.sampled_at <= result.snapshot.as_of)
        );
        assert!(
            result
                .snapshot
                .sources
                .iter()
                .find(|source| source.source == "app_server")
                .is_some_and(|source| matches!(source.status.as_str(), "ok" | "partial"))
        );
        assert!(
            !result
                .snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("dated after the snapshot as-of"))
        );
    });
}

#[cfg(unix)]
#[test]
fn preserves_limits_when_reset_credit_count_is_invalid() {
    let script = r#"#!/bin/sh
IFS= read -r initialize || exit 71
printf '%s\n' '{"id":1,"result":{"userAgent":"mock"}}'
IFS= read -r initialized || exit 72
IFS= read -r limits || exit 73
IFS= read -r usage || exit 74
printf '%s\n' '{"id":3,"error":{"code":-32601,"message":"usage disabled"}}'
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":23,"windowDurationMins":300}},"rateLimitsByLimitId":null,"rateLimitResetCredits":{"availableCount":-1}}}'
"#;

    with_mock_codex(script, |directory| {
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_secs(2),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();

        assert_eq!(snapshot.limits.len(), 1);
        assert_eq!(snapshot.limits[0].limit_id, "codex");
        assert!(snapshot.rate_limit_reset_credits.is_none());
        assert!(snapshot.rate_limit_reset_credits_partial);
        assert!(snapshot.errors.is_empty());
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| { warning.contains("rateLimitResetCredits.availableCount") })
        );
    });
}

#[cfg(unix)]
#[test]
fn preserves_reset_credit_count_when_the_details_container_is_invalid() {
    let script = r#"#!/bin/sh
IFS= read -r initialize || exit 81
printf '%s\n' '{"id":1,"result":{"userAgent":"mock"}}'
IFS= read -r initialized || exit 82
IFS= read -r limits || exit 83
IFS= read -r usage || exit 84
printf '%s\n' '{"id":3,"error":{"code":-32601,"message":"usage disabled"}}'
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":23,"windowDurationMins":300}},"rateLimitsByLimitId":null,"rateLimitResetCredits":{"availableCount":2,"credits":true}}}'
"#;

    with_mock_codex(script, |directory| {
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_secs(2),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();

        assert_eq!(snapshot.limits.len(), 1);
        let reset_credits = snapshot.rate_limit_reset_credits.as_ref().unwrap();
        assert_eq!(reset_credits.available_count, 2);
        assert_eq!(reset_credits.credits, None);
        assert!(snapshot.rate_limit_reset_credits_partial);
        assert!(snapshot.errors.is_empty());
        assert!(snapshot.warnings.iter().any(|warning| {
            warning.contains("rateLimitResetCredits.credits must be an array or null")
        }));
    });
}

#[cfg(unix)]
#[test]
fn preserves_limits_when_the_optional_usage_rpc_stalls() {
    let script = r#"#!/bin/sh
IFS= read -r initialize || exit 61
printf '%s\n' '{"id":1,"result":{"userAgent":"mock"}}'
IFS= read -r initialized || exit 62
IFS= read -r limits || exit 63
IFS= read -r usage || exit 64
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":17,"windowDurationMins":300,"resetsAt":1783834200}},"rateLimitsByLimitId":null}}'
while IFS= read -r ignored; do :; done
"#;

    with_mock_codex(script, |directory| {
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_millis(100),
            ..CollectConfig::default()
        };

        let snapshot = fetch_account_snapshot(&config).unwrap();

        assert_eq!(snapshot.limits.len(), 1);
        assert_eq!(
            snapshot.limits[0].primary.as_ref().unwrap().used_percent,
            17.0
        );
        assert!(snapshot.usage.is_none());
        assert!(snapshot.errors.is_empty());
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.contains("usage/read did not complete"))
        );
    });
}

#[cfg(unix)]
#[test]
fn initialization_timeout_returns_promptly() {
    let script = r#"#!/bin/sh
IFS= read -r initialize || exit 51
printf '%s\n' 'mock app-server stalled' >&2
while IFS= read -r ignored; do :; done
"#;

    with_mock_codex(script, |directory| {
        let trace =
            codex_usage_monit::startup::StartupTrace::enabled(Instant::now(), None).unwrap();
        let config = CollectConfig {
            codex_home: directory.join("home"),
            app_server_timeout: Duration::from_millis(80),
            startup_trace: trace.clone(),
            ..CollectConfig::default()
        };
        let started = Instant::now();

        let error = fetch_account_snapshot(&config).unwrap_err();
        trace.stop();

        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let report = trace.report();
        assert!(report.events.iter().any(|event| {
            event.stage == "app_server.initialize" && event.detail == "status=error"
        }));
        assert!(
            report
                .events
                .iter()
                .any(|event| event.stage == "app_server.shutdown")
        );
        assert!(
            report.events.iter().any(|event| {
                event.stage == "app_server.total" && event.detail == "status=error"
            })
        );
    });
}
