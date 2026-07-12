use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, AccountTokenUsage, CreditsSnapshot, DailyTokenBucket, LimitBucket,
    LimitWindow, Provenance,
};

const INITIALIZE_ID: u64 = 1;
const RATE_LIMITS_ID: u64 = 2;
const ACCOUNT_USAGE_ID: u64 = 3;
const STDERR_LIMIT: usize = 32 * 1024;

enum ReaderEvent {
    Message(Value),
    Malformed(String),
    Eof,
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn terminate_and_reap(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

/// Fetches the read-only account quota and token-activity snapshot from Codex.
///
/// Authentication remains owned by `codex app-server`; this adapter does not read
/// `auth.json` and does not call any write or control methods.
pub fn fetch_account_snapshot(config: &CollectConfig) -> Result<AccountSnapshot> {
    if config.offline {
        return Ok(AccountSnapshot {
            warnings: vec!["Codex app-server collection is disabled in offline mode".to_string()],
            ..AccountSnapshot::default()
        });
    }

    let child = Command::new("codex")
        .args(["app-server", "--stdio"])
        .env("CODEX_HOME", &config.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `codex app-server --stdio`")?;

    let mut child = ChildGuard { child };
    let stdin = child
        .child
        .stdin
        .take()
        .context("codex app-server did not expose stdin")?;
    let stdout = child
        .child
        .stdout
        .take()
        .context("codex app-server did not expose stdout")?;
    let stderr = child
        .child
        .stderr
        .take()
        .context("codex app-server did not expose stderr")?;

    let (reader_tx, reader_rx) = mpsc::channel();
    thread::spawn(move || read_stdout(stdout, reader_tx));

    let stderr_output = Arc::new(Mutex::new(String::new()));
    let stderr_writer = Arc::clone(&stderr_output);
    thread::spawn(move || capture_stderr(stderr, stderr_writer));

    let deadline = Instant::now()
        .checked_add(config.app_server_timeout)
        .context("app-server timeout exceeds the platform's supported range")?;
    let mut stdin = stdin;
    let mut protocol_warnings = Vec::new();

    let result = (|| {
        write_message(
            &mut stdin,
            &json!({
                "method": "initialize",
                "id": INITIALIZE_ID,
                "params": {
                    "clientInfo": {
                        "name": "codex-usage-monit",
                        "title": "Codex Usage Monitor",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": null
                }
            }),
        )
        .context("failed to initialize codex app-server")?;

        match wait_for_response(
            INITIALIZE_ID,
            "initialize",
            deadline,
            &reader_rx,
            &mut protocol_warnings,
            &stderr_output,
        )? {
            Ok(_) => {}
            Err(message) => bail!("codex app-server initialize failed: {message}"),
        }

        write_message(&mut stdin, &json!({ "method": "initialized" }))?;
        write_message(
            &mut stdin,
            &json!({ "method": "account/rateLimits/read", "id": RATE_LIMITS_ID }),
        )?;
        write_message(
            &mut stdin,
            &json!({ "method": "account/usage/read", "id": ACCOUNT_USAGE_ID }),
        )?;

        let mut rate_limits = None;
        let mut account_usage = None;
        while rate_limits.is_none() || account_usage.is_none() {
            let event = match recv_event(deadline, &reader_rx, "account snapshot", &stderr_output) {
                Ok(event) => event,
                Err(error) if rate_limits.is_some() => {
                    protocol_warnings.push(format!(
                        "account/usage/read did not complete after rate limits were received: {error:#}"
                    ));
                    break;
                }
                Err(error) => return Err(error),
            };
            match event {
                ReaderEvent::Message(message) => {
                    if response_id(&message) == Some(RATE_LIMITS_ID) && rate_limits.is_none() {
                        rate_limits = Some(response_payload(&message));
                    } else if response_id(&message) == Some(ACCOUNT_USAGE_ID)
                        && account_usage.is_none()
                    {
                        account_usage = Some(response_payload(&message));
                    }
                }
                ReaderEvent::Malformed(message) => protocol_warnings.push(message),
                ReaderEvent::Eof => {
                    if rate_limits.is_some() {
                        protocol_warnings.push(format!(
                            "codex app-server closed stdout before returning account/usage/read{}",
                            stderr_suffix(&stderr_output)
                        ));
                        break;
                    }
                    bail!(
                        "codex app-server closed stdout before returning the account snapshot{}",
                        stderr_suffix(&stderr_output)
                    );
                }
            }
        }

        let mut snapshot = AccountSnapshot {
            warnings: protocol_warnings,
            ..AccountSnapshot::default()
        };
        match rate_limits.expect("rate-limit response must be present") {
            Ok(result) => match parse_rate_limits_result(&result, Utc::now()) {
                Ok(limits) => snapshot.limits = limits,
                Err(error) => snapshot.errors.push(format!(
                    "account/rateLimits/read returned invalid data: {error:#}"
                )),
            },
            Err(message) => snapshot
                .errors
                .push(format!("account/rateLimits/read failed: {message}")),
        }
        if let Some(account_usage) = account_usage {
            match account_usage {
                Ok(result) => match parse_account_usage_result(&result) {
                    Ok(usage) => snapshot.usage = Some(usage),
                    Err(error) => snapshot.warnings.push(format!(
                        "account/usage/read returned invalid data: {error:#}"
                    )),
                },
                Err(message) => snapshot
                    .warnings
                    .push(format!("account/usage/read failed: {message}")),
            }
        }

        Ok(snapshot)
    })();

    drop(stdin);
    drop(child);
    result
}

/// Parses the `result` object returned by `account/rateLimits/read`.
pub fn parse_rate_limits_result(result: &Value, as_of: DateTime<Utc>) -> Result<Vec<LimitBucket>> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("rate-limit result must be an object")?;

    let mut snapshots = BTreeMap::<String, &Value>::new();
    match object.get("rateLimitsByLimitId") {
        Some(Value::Object(by_id)) => {
            for (limit_id, snapshot) in by_id {
                if !snapshot.is_null() {
                    snapshots.insert(limit_id.clone(), snapshot);
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => bail!("rateLimitsByLimitId must be an object or null"),
    }

    if snapshots.is_empty() {
        if let Some(snapshot) = object.get("rateLimits").filter(|value| !value.is_null()) {
            let key = optional_string(
                snapshot
                    .as_object()
                    .context("rateLimits must be an object")?,
                "limitId",
            )?
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());
            snapshots.insert(key, snapshot);
        } else if object.contains_key("primary") || object.contains_key("secondary") {
            let key = optional_string(object, "limitId")?
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "default".to_string());
            snapshots.insert(key, result);
        }
    }

    if snapshots.is_empty() {
        bail!("rate-limit result did not contain any limit snapshots");
    }

    snapshots
        .into_iter()
        .map(|(fallback_id, snapshot)| parse_limit_bucket(snapshot, &fallback_id, as_of))
        .collect()
}

/// Parses the `result` object returned by `account/usage/read`.
pub fn parse_account_usage_result(result: &Value) -> Result<AccountTokenUsage> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("account-usage result must be an object")?;
    let summary = object
        .get("summary")
        .and_then(Value::as_object)
        .context("account-usage result is missing summary")?;

    let daily_usage_buckets = match object.get("dailyUsageBuckets") {
        Some(Value::Array(buckets)) => buckets
            .iter()
            .enumerate()
            .map(|(index, bucket)| {
                let bucket = bucket
                    .as_object()
                    .with_context(|| format!("dailyUsageBuckets[{index}] must be an object"))?;
                Ok(DailyTokenBucket {
                    start_date: required_string(bucket, "startDate").with_context(|| {
                        format!("dailyUsageBuckets[{index}].startDate is invalid")
                    })?,
                    tokens: required_u64(bucket, "tokens")
                        .with_context(|| format!("dailyUsageBuckets[{index}].tokens is invalid"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => bail!("dailyUsageBuckets must be an array or null"),
    };

    Ok(AccountTokenUsage {
        lifetime_tokens: optional_u64(summary, "lifetimeTokens")?,
        peak_daily_tokens: optional_u64(summary, "peakDailyTokens")?,
        longest_running_turn_sec: optional_u64(summary, "longestRunningTurnSec")?,
        current_streak_days: optional_u64(summary, "currentStreakDays")?,
        longest_streak_days: optional_u64(summary, "longestStreakDays")?,
        daily_usage_buckets,
    })
}

fn parse_limit_bucket(
    value: &Value,
    fallback_id: &str,
    as_of: DateTime<Utc>,
) -> Result<LimitBucket> {
    let object = value
        .as_object()
        .context("rate-limit snapshot must be an object")?;
    let limit_id = optional_string(object, "limitId")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_id.to_string());

    Ok(LimitBucket {
        limit_id,
        limit_name: optional_string(object, "limitName")?,
        plan_type: optional_string(object, "planType")?,
        primary: optional_window(object, "primary")?,
        secondary: optional_window(object, "secondary")?,
        credits: optional_credits(object)?,
        rate_limit_reached_type: optional_string(object, "rateLimitReachedType")?,
        provenance: Provenance::ServerSnapshot,
        as_of,
    })
}

fn optional_window(object: &Map<String, Value>, key: &str) -> Result<Option<LimitWindow>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let window = value
        .as_object()
        .with_context(|| format!("{key} must be an object or null"))?;
    let used_percent = required_f64(window, "usedPercent")
        .with_context(|| format!("{key}.usedPercent is invalid"))?;
    if !used_percent.is_finite() {
        bail!("{key}.usedPercent must be finite");
    }

    Ok(Some(LimitWindow::new(
        used_percent,
        optional_i64(window, "windowDurationMins")?,
        optional_timestamp(window, "resetsAt")?,
    )))
}

fn optional_credits(object: &Map<String, Value>) -> Result<Option<CreditsSnapshot>> {
    let Some(value) = object.get("credits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let credits = value
        .as_object()
        .context("credits must be an object or null")?;
    Ok(Some(CreditsSnapshot {
        has_credits: required_bool(credits, "hasCredits")?,
        unlimited: required_bool(credits, "unlimited")?,
        balance: optional_scalar_string(credits, "balance")?,
    }))
}

fn unwrap_result(value: &Value) -> &Value {
    value.get("result").unwrap_or(value)
}

fn response_id(message: &Value) -> Option<u64> {
    if message.get("method").is_some() {
        return None;
    }
    message.get("id").and_then(Value::as_u64)
}

fn response_payload(message: &Value) -> std::result::Result<Value, String> {
    if let Some(error) = message.get("error") {
        return Err(format_rpc_error(error));
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| "response contained neither result nor error".to_string())
}

fn format_rpc_error(error: &Value) -> String {
    let Some(object) = error.as_object() else {
        return error.to_string();
    };
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown JSON-RPC error");
    match object.get("code") {
        Some(code) => format!("{message} (code {code})"),
        None => message.to_string(),
    }
}

fn write_message(stdin: &mut impl Write, message: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, message).context("failed to encode app-server request")?;
    stdin
        .write_all(b"\n")
        .context("failed to write app-server request")?;
    stdin.flush().context("failed to flush app-server request")
}

fn wait_for_response(
    id: u64,
    operation: &str,
    deadline: Instant,
    receiver: &mpsc::Receiver<ReaderEvent>,
    warnings: &mut Vec<String>,
    stderr: &Arc<Mutex<String>>,
) -> Result<std::result::Result<Value, String>> {
    loop {
        match recv_event(deadline, receiver, operation, stderr)? {
            ReaderEvent::Message(message) if response_id(&message) == Some(id) => {
                return Ok(response_payload(&message));
            }
            ReaderEvent::Message(_) => {}
            ReaderEvent::Malformed(message) => warnings.push(message),
            ReaderEvent::Eof => {
                bail!(
                    "codex app-server closed stdout while waiting for {operation}{}",
                    stderr_suffix(stderr)
                );
            }
        }
    }
}

fn recv_event(
    deadline: Instant,
    receiver: &mpsc::Receiver<ReaderEvent>,
    operation: &str,
    stderr: &Arc<Mutex<String>>,
) -> Result<ReaderEvent> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            anyhow!(
                "timed out waiting for codex app-server {operation}{}",
                stderr_suffix(stderr)
            )
        })?;
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => anyhow!(
                "timed out waiting for codex app-server {operation}{}",
                stderr_suffix(stderr)
            ),
            mpsc::RecvTimeoutError::Disconnected => anyhow!(
                "codex app-server stdout reader stopped while waiting for {operation}{}",
                stderr_suffix(stderr)
            ),
        })
}

fn read_stdout(stdout: impl Read, sender: mpsc::Sender<ReaderEvent>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = sender.send(ReaderEvent::Eof);
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let event = match serde_json::from_str(trimmed) {
                    Ok(value) => ReaderEvent::Message(value),
                    Err(error) => ReaderEvent::Malformed(format!(
                        "ignored malformed codex app-server output: {error}"
                    )),
                };
                if sender.send(event).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(ReaderEvent::Malformed(format!(
                    "failed to read codex app-server stdout: {error}"
                )));
                let _ = sender.send(ReaderEvent::Eof);
                break;
            }
        }
    }
}

fn capture_stderr(mut stderr: impl Read, output: Arc<Mutex<String>>) {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let available = STDERR_LIMIT.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..count.min(available)]);
                if let Ok(mut destination) = output.lock() {
                    *destination = String::from_utf8_lossy(&captured).trim().to_string();
                }
            }
        }
    }
}

fn stderr_suffix(stderr: &Arc<Mutex<String>>) -> String {
    let output = stderr
        .lock()
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if output.is_empty() {
        String::new()
    } else {
        format!(": {output}")
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a string or null"),
    }
}

fn optional_scalar_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Number(value)) => Ok(Some(value.to_string())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a string, number, or null"),
    }
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String> {
    optional_string(object, key)?.ok_or_else(|| anyhow!("{key} is required"))
}

fn required_bool(object: &Map<String, Value>, key: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("{key} must be a boolean"))
}

fn required_f64(object: &Map<String, Value>, key: &str) -> Result<f64> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| anyhow!("{key} is outside the supported numeric range")),
        Some(Value::String(value)) => value
            .parse::<f64>()
            .with_context(|| format!("{key} must be numeric")),
        _ => bail!("{key} is required and must be numeric"),
    }
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be a non-negative integer")),
        Some(Value::String(value)) => value
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("{key} must be a non-negative integer")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be a non-negative integer or null"),
    }
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64> {
    optional_u64(object, key)?.ok_or_else(|| anyhow!("{key} is required"))
}

fn optional_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be an integer")),
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map(Some)
            .with_context(|| format!("{key} must be an integer")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be an integer or null"),
    }
}

fn optional_timestamp(object: &Map<String, Value>, key: &str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(value) = value.as_str() {
        if let Ok(seconds) = value.parse::<i64>() {
            return timestamp_from_integer(seconds, key).map(Some);
        }
        return DateTime::parse_from_rfc3339(value)
            .map(|timestamp| Some(timestamp.with_timezone(&Utc)))
            .with_context(|| format!("{key} must be Unix seconds or an RFC 3339 timestamp"));
    }
    let seconds = value
        .as_i64()
        .ok_or_else(|| anyhow!("{key} must be Unix seconds or null"))?;
    timestamp_from_integer(seconds, key).map(Some)
}

fn timestamp_from_integer(value: i64, key: &str) -> Result<DateTime<Utc>> {
    let (seconds, nanos) = if value.unsigned_abs() >= 1_000_000_000_000 {
        (
            value.div_euclid(1_000),
            value.rem_euclid(1_000) as u32 * 1_000_000,
        )
    } else {
        (value, 0)
    };
    DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| anyhow!("{key} is outside the supported timestamp range"))
}
