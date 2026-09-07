use std::fs;
use std::process::Command;

use chrono::{Duration, Utc};
use serde_json::Value;
use tempfile::tempdir;

fn monitor_command(config_dir: &std::path::Path, codex_home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-usage-monit"));
    command
        .env("CODEX_USAGE_MONIT_CONFIG_DIR", config_dir)
        .arg("--offline")
        .arg("--no-rollout-cache")
        .arg("--codex-home")
        .arg(codex_home)
        .arg("snapshot")
        .arg("--format")
        .arg("json")
        .arg("--compact");
    command
}

fn write_custom_catalog(config_dir: &std::path::Path) {
    fs::create_dir_all(config_dir).unwrap();
    fs::write(
        config_dir.join("model-catalog.json"),
        r#"{
          "version": 1,
          "estimatorRevision": 77,
          "apiPricingCatalogRevision": 88,
          "ratesAsOf": "2099-01-02",
          "sourceUrl": "https://example.invalid/custom-catalog",
          "longContextInputThreshold": 123456,
          "creditFallbackModel": "fallback-model",
          "models": [{
            "id": "custom-model",
            "aliases": ["custom-model-latest"],
            "credit": {
              "standard": {"input": 10, "cachedInput": 10, "output": 10},
              "fast": {"input": 20, "cachedInput": 20, "output": 20}
            },
            "api": {
              "standard": {
                "short": {"input": 3, "cachedInput": 1, "output": 9},
                "long": {"mode": "flat"}
              }
            }
          }, {
            "id": "fallback-model",
            "credit": {
              "standard": {"input": 1, "cachedInput": 1, "output": 1},
              "fast": {"input": 2, "cachedInput": 2, "output": 2}
            }
          }]
        }"#,
    )
    .unwrap();
}

fn write_catalog_rollout(codex_home: &std::path::Path) {
    let sessions = codex_home.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let started_at = Utc::now() - Duration::minutes(10);
    let reset_at = (Utc::now() + Duration::hours(1)).timestamp();
    let usage = |input_tokens: u64| {
        serde_json::json!({
            "input_tokens": input_tokens,
            "cached_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": input_tokens
        })
    };
    let event = |seconds: i64, payload: Value| {
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(seconds),
            "type": "event_msg",
            "payload": payload
        })
    };
    let rate_limits = || {
        serde_json::json!({
            "limit_id": "codex",
            "primary": {
                "used_percent": 55,
                "window_minutes": 300,
                "resets_at": reset_at
            }
        })
    };
    let records = [
        serde_json::json!({
            "timestamp": started_at,
            "type": "session_meta",
            "payload": {
                "id": "catalog-calculation-thread",
                "timestamp": started_at,
                "cwd": "/work/catalog-calculation"
            }
        }),
        event(
            1,
            serde_json::json!({
                "type": "thread_settings_applied",
                "thread_settings": {"service_tier": "default"}
            }),
        ),
        event(
            2,
            serde_json::json!({"type": "task_started", "turn_id": "alias-turn"}),
        ),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(3),
            "type": "turn_context",
            "payload": {
                "turn_id": "alias-turn",
                "model": " CUSTOM-MODEL-LATEST "
            }
        }),
        event(
            4,
            serde_json::json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage(1_000_000),
                    "last_token_usage": usage(1_000_000)
                },
                "rate_limits": rate_limits()
            }),
        ),
        event(
            5,
            serde_json::json!({"type": "task_complete", "turn_id": "alias-turn"}),
        ),
        event(
            6,
            serde_json::json!({"type": "task_started", "turn_id": "fallback-turn"}),
        ),
        serde_json::json!({
            "timestamp": started_at + Duration::seconds(7),
            "type": "turn_context",
            "payload": {"turn_id": "fallback-turn", "model": "unknown-model"}
        }),
        event(
            8,
            serde_json::json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage(2_000_000),
                    "last_token_usage": usage(1_000_000)
                },
                "rate_limits": rate_limits()
            }),
        ),
        event(
            9,
            serde_json::json!({"type": "task_complete", "turn_id": "fallback-turn"}),
        ),
    ];
    let contents = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(sessions.join("rollout-model-catalog.jsonl"), contents).unwrap();
}

#[test]
fn configured_catalog_metadata_is_used_by_a_fresh_process() {
    let directory = tempdir().unwrap();
    let config_dir = directory.path().join("config");
    let codex_home = directory.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();
    write_custom_catalog(&config_dir);

    let output = monitor_command(&config_dir, &codex_home).output().unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["apiPricing"]["catalogRevision"], 88);
    assert_eq!(document["apiPricing"]["ratesAsOf"], "2099-01-02");
    assert_eq!(
        document["apiPricing"]["sourceUrl"],
        "https://example.invalid/custom-catalog"
    );
}

#[test]
fn configured_catalog_changes_alias_fallback_and_api_calculation_in_a_fresh_process() {
    let directory = tempdir().unwrap();
    let config_dir = directory.path().join("config");
    let codex_home = directory.path().join("codex");
    write_custom_catalog(&config_dir);
    write_catalog_rollout(&codex_home);

    let output = monitor_command(&config_dir, &codex_home).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "offline/fallback fixture is intentionally partial; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    let five_hour = document["windowAnalyses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|analysis| analysis["durationMins"] == 300)
        .expect("fixture must produce a five-hour analysis");
    assert_eq!(five_hour["apiPricing"]["catalogRevision"], 88);

    let turns = five_hour["turns"].as_array().unwrap();
    let usage_for = |turn_id: &str| {
        &turns
            .iter()
            .find(|turn| turn["turnId"] == turn_id)
            .unwrap_or_else(|| panic!("missing {turn_id}: {turns:?}"))["usage"]
    };
    let alias = usage_for("alias-turn");
    let fallback = usage_for("fallback-turn");

    // Both turns contain the same raw token count. The configured 10:1 credit
    // rates therefore split a 55% gauge into exactly 50% and 5%, proving that
    // the alias matched while the unknown model used creditFallbackModel.
    assert_eq!(alias["tokenUsage"]["totalTokens"], 1_000_000);
    assert_eq!(fallback["tokenUsage"]["totalTokens"], 1_000_000);
    let estimated = |usage: &Value| usage["estimatedQuotaPercent"].as_f64().unwrap();
    assert!((estimated(alias) - 50.0).abs() < 1e-9);
    assert!((estimated(fallback) - 5.0).abs() < 1e-9);

    // The alias's configured $3 / 1M input price is used end-to-end. The
    // unknown model does not borrow the credit fallback for API pricing.
    assert_eq!(
        alias["apiEquivalentCost"]["minimumPicoUsd"],
        "3000000000000"
    );
    assert_eq!(alias["apiEquivalentCost"]["pricedTokens"], 1_000_000);
    assert_eq!(fallback["apiEquivalentCost"]["minimumPicoUsd"], "0");
    assert_eq!(fallback["apiEquivalentCost"]["observedTokens"], 1_000_000);
    assert_eq!(fallback["apiEquivalentCost"]["pricedTokens"], 0);
}

#[test]
fn frozen_service_config_path_uses_its_sibling_catalog() {
    let directory = tempdir().unwrap();
    let environment_config = directory.path().join("environment-config");
    let service_config = directory.path().join("service-config");
    let codex_home = directory.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();
    fs::create_dir_all(&environment_config).unwrap();
    write_custom_catalog(&service_config);

    let output = monitor_command(&environment_config, &codex_home)
        .arg("--service-remotes-config")
        .arg(service_config.join("remotes.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success() || output.status.code() == Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["apiPricing"]["catalogRevision"], 88);
}

#[test]
fn malformed_existing_catalog_fails_instead_of_falling_back() {
    let directory = tempdir().unwrap();
    let config_dir = directory.path().join("config");
    let codex_home = directory.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("model-catalog.json"), b"not json\n").unwrap();

    let output = monitor_command(&config_dir, &codex_home).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unable to load model catalog"), "{stderr}");
    assert!(stderr.contains("invalid model catalog JSON"), "{stderr}");
}

#[test]
fn missing_catalog_uses_the_bundled_metadata() {
    let directory = tempdir().unwrap();
    let config_dir = directory.path().join("empty-config");
    let codex_home = directory.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();
    fs::create_dir_all(&config_dir).unwrap();

    let output = monitor_command(&config_dir, &codex_home).output().unwrap();
    assert!(output.status.success() || output.status.code() == Some(2));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["apiPricing"]["catalogRevision"], 3);
    assert_eq!(document["apiPricing"]["ratesAsOf"], "2026-09-07");
}
