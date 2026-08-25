use std::collections::BTreeMap;
#[cfg(windows)]
use std::env;
use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(windows)]
use std::path::{Path, PathBuf};
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
    LimitWindow, Provenance, RateLimitResetCredit, RateLimitResetCreditsSnapshot,
};
use crate::startup::StartupTrace;

const INITIALIZE_ID: u64 = 1;
const RATE_LIMITS_ID: u64 = 2;
const ACCOUNT_USAGE_ID: u64 = 3;
const STDERR_LIMIT: usize = 32 * 1024;
const STDERR_DIAGNOSTIC_LIMIT: usize = 2 * 1024;
const RPC_ERROR_MESSAGE_LIMIT: usize = 512;
const DESKTOP_CLI_RESOURCE_DIAGNOSTIC: &str = "Codex Desktop packaged resource";
#[cfg(windows)]
const DESKTOP_PACKAGE_PREFIX: &str = "OpenAI.Codex_";

enum ReaderEvent {
    Message(Value),
    Malformed(String),
    Eof,
}

struct ChildGuard {
    child: Child,
    startup_trace: StartupTrace,
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
        let shutdown_span = self.startup_trace.span("app_server.shutdown");
        self.terminate_and_reap();
        shutdown_span.finish("status=reaped");
    }
}

/// Fetches the read-only account quota and token-activity snapshot from Codex.
///
/// Authentication remains owned by `codex app-server`; this adapter does not read
/// `auth.json` and does not call any write or control methods.
pub fn fetch_account_snapshot(config: &CollectConfig) -> Result<AccountSnapshot> {
    let total_span = config.startup_trace.span("app_server.total");
    if config.offline {
        total_span.finish("status=offline");
        return Ok(AccountSnapshot {
            warnings: vec!["Codex app-server collection is disabled in offline mode".to_string()],
            ..AccountSnapshot::default()
        });
    }

    let spawn_span = config.startup_trace.span("app_server.spawn");
    let mut command = codex_command(config);
    if let Some(path) = config.app_server_path.as_deref() {
        command.env("PATH", path);
    }
    let program = command.get_program().to_owned();
    let child = command
        .arg("app-server")
        .env("CODEX_HOME", &config.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| app_server_spawn_error(&program, error))
        .context("failed to spawn `codex app-server`")?;
    spawn_span.finish("command=codex_app_server");

    let io_span = config.startup_trace.span("app_server.io_setup");
    let mut child = ChildGuard {
        child,
        startup_trace: config.startup_trace.clone(),
    };
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
    io_span.finish_with(|| format!("timeout_ms={}", config.app_server_timeout.as_millis()));

    let result = (|| {
        let initialize_span = config.startup_trace.span("app_server.initialize");
        let initialize_result = (|| -> Result<()> {
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
                Ok(_) => Ok(()),
                Err(message) => bail!("codex app-server initialize failed: {message}"),
            }
        })();
        if let Err(error) = initialize_result {
            initialize_span.finish("status=error");
            return Err(error);
        }
        initialize_span.finish("status=ok");

        let account_span = config.startup_trace.span("app_server.account_reads");
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
                Err(error) => {
                    account_span.finish("status=error kind=receive");
                    return Err(error);
                }
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
                    account_span.finish("status=error kind=eof");
                    bail!(
                        "codex app-server closed stdout before returning the account snapshot{}",
                        stderr_suffix(&stderr_output)
                    );
                }
            }
        }
        account_span.finish_with(|| {
            format!(
                "status={} rate_limits={} usage={} warnings={}",
                if account_usage.is_some() {
                    "ok"
                } else {
                    "partial"
                },
                rate_limits.is_some(),
                account_usage.is_some(),
                protocol_warnings.len()
            )
        });

        let parse_span = config.startup_trace.span("app_server.parse_responses");
        let mut snapshot = AccountSnapshot {
            warnings: protocol_warnings,
            ..AccountSnapshot::default()
        };
        match rate_limits.expect("rate-limit response must be present") {
            Ok(result) => {
                let as_of = Utc::now();
                match parse_rate_limits_result(&result, as_of) {
                    Ok(limits) => snapshot.limits = limits,
                    Err(error) => snapshot.errors.push(format!(
                        "account/rateLimits/read returned invalid data: {error:#}"
                    )),
                }
                match parse_rate_limit_reset_credits_result_lossy(&result, as_of) {
                    Ok((reset_credits, detail_errors)) => {
                        snapshot.rate_limit_reset_credits = reset_credits;
                        if !detail_errors.is_empty() {
                            snapshot.rate_limit_reset_credits_partial = true;
                            snapshot.warnings.extend(detail_errors.into_iter().map(|error| {
                                format!(
                                    "account/rateLimits/read ignored invalid reset credit detail: {error}"
                                )
                            }));
                        }
                    }
                    Err(error) => {
                        snapshot.rate_limit_reset_credits_partial = true;
                        snapshot.warnings.push(format!(
                            "account/rateLimits/read returned invalid reset credits: {error:#}"
                        ));
                    }
                }
            }
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
                Err(message) if optional_usage_rpc_unavailable(&message) => {}
                Err(message) => snapshot
                    .warnings
                    .push(format!("account/usage/read failed: {message}")),
            }
        }

        parse_span.finish_with(|| {
            format!(
                "buckets={} reset_credits={} usage={} warnings={} errors={}",
                snapshot.limits.len(),
                snapshot.rate_limit_reset_credits.is_some(),
                snapshot.usage.is_some(),
                snapshot.warnings.len(),
                snapshot.errors.len()
            )
        });

        Ok(snapshot)
    })();

    drop(stdin);
    drop(child);
    total_span.finish_with(|| format!("status={}", if result.is_ok() { "ok" } else { "error" }));
    result
}

fn codex_command(config: &CollectConfig) -> Command {
    if let Some(codex_bin) = config.codex_bin.as_deref() {
        return Command::new(codex_bin);
    }
    #[cfg(not(windows))]
    {
        Command::new("codex")
    }
    #[cfg(windows)]
    {
        let path = config
            .app_server_path
            .clone()
            .or_else(|| env::var_os("PATH"))
            .unwrap_or_default();
        let monitor_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let discovered =
            crate::session_launch::resolve_executable("codex", None, &path, &monitor_cwd).ok();
        Command::new(select_windows_codex_program(
            discovered,
            installed_windows_codex_cli(),
        ))
    }
}

#[cfg(windows)]
fn installed_windows_codex_cli() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin")
        .join("codex.exe");
    path.is_file().then_some(path)
}

#[cfg(windows)]
fn select_windows_codex_program(
    discovered: Option<PathBuf>,
    installed: Option<PathBuf>,
) -> PathBuf {
    match (discovered, installed) {
        (Some(discovered), Some(installed)) if is_desktop_codex_resource(&discovered) => installed,
        (Some(discovered), _) => discovered,
        (None, Some(installed)) => installed,
        (None, None) => PathBuf::from("codex"),
    }
}

fn app_server_spawn_error(program: &OsStr, error: io::Error) -> anyhow::Error {
    #[cfg(windows)]
    {
        let path = Path::new(program);
        if error.kind() == io::ErrorKind::PermissionDenied
            && error.raw_os_error() == Some(5)
            && is_desktop_codex_resource(path)
        {
            return anyhow!(
                "found the {DESKTOP_CLI_RESOURCE_DIAGNOSTIC} at {}; Windows cannot launch it as a standalone Codex CLI. Install and sign in to the Codex CLI, use --codex-bin to select a runnable `codex.cmd` or `codex.exe`, or use --offline to monitor local rollout data: {error}",
                path.display()
            );
        }
    }

    error.into()
}

#[cfg(windows)]
fn is_desktop_codex_resource(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    if !path_component_equals(file_name, "codex") && !path_component_equals(file_name, "codex.exe")
    {
        return false;
    }

    let Some(resources) = path.parent() else {
        return false;
    };
    let Some(app) = resources.parent() else {
        return false;
    };
    let Some(package) = app.parent() else {
        return false;
    };
    let Some(windows_apps) = package.parent() else {
        return false;
    };

    path_component_equals(resources.file_name().unwrap_or_default(), "resources")
        && path_component_equals(app.file_name().unwrap_or_default(), "app")
        && package
            .file_name()
            .map(|name| name.to_string_lossy())
            .and_then(|name| name.get(..DESKTOP_PACKAGE_PREFIX.len()).map(str::to_owned))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(DESKTOP_PACKAGE_PREFIX))
        && path_component_equals(windows_apps.file_name().unwrap_or_default(), "WindowsApps")
}

#[cfg(windows)]
fn path_component_equals(value: &OsStr, expected: &str) -> bool {
    value.to_string_lossy().eq_ignore_ascii_case(expected)
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

/// Parses the reset opportunities returned alongside `account/rateLimits/read`.
pub fn parse_rate_limit_reset_credits_result(
    result: &Value,
    as_of: DateTime<Utc>,
) -> Result<Option<RateLimitResetCreditsSnapshot>> {
    let Some(reset_credits) = rate_limit_reset_credits_object(result)? else {
        return Ok(None);
    };
    let available_count = required_u64(reset_credits, "availableCount")
        .context("rateLimitResetCredits.availableCount is invalid")?;
    if available_count > i64::MAX as u64 {
        bail!("rateLimitResetCredits.availableCount exceeds the protocol int64 range");
    }
    let credits = optional_reset_credits(reset_credits)?;

    Ok(Some(RateLimitResetCreditsSnapshot {
        available_count,
        credits,
        provenance: Provenance::ServerSnapshot,
        as_of,
    }))
}

fn parse_rate_limit_reset_credits_result_lossy(
    result: &Value,
    as_of: DateTime<Utc>,
) -> Result<(Option<RateLimitResetCreditsSnapshot>, Vec<String>)> {
    let Some(reset_credits) = rate_limit_reset_credits_object(result)? else {
        return Ok((None, Vec::new()));
    };
    let available_count = required_u64(reset_credits, "availableCount")
        .context("rateLimitResetCredits.availableCount is invalid")?;
    if available_count > i64::MAX as u64 {
        bail!("rateLimitResetCredits.availableCount exceeds the protocol int64 range");
    }

    let mut detail_errors = Vec::new();
    let credits = match optional_reset_credit_values(reset_credits) {
        Ok(None) => None,
        Ok(Some(values)) => {
            let mut credits = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                match parse_reset_credit(value, index) {
                    Ok(credit) => credits.push(credit),
                    Err(error) => detail_errors.push(format!("{error:#}")),
                }
            }
            Some(credits)
        }
        Err(error) => {
            detail_errors.push(format!("{error:#}"));
            None
        }
    };

    Ok((
        Some(RateLimitResetCreditsSnapshot {
            available_count,
            credits,
            provenance: Provenance::ServerSnapshot,
            as_of,
        }),
        detail_errors,
    ))
}

fn rate_limit_reset_credits_object(result: &Value) -> Result<Option<&Map<String, Value>>> {
    let result = unwrap_result(result);
    let object = result
        .as_object()
        .context("rate-limit result must be an object")?;
    let Some(value) = object.get("rateLimitResetCredits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_object()
        .map(Some)
        .context("rateLimitResetCredits must be an object or null")
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

fn optional_reset_credits(
    object: &Map<String, Value>,
) -> Result<Option<Vec<RateLimitResetCredit>>> {
    let Some(credits) = optional_reset_credit_values(object)? else {
        return Ok(None);
    };
    credits
        .iter()
        .enumerate()
        .map(|(index, credit)| parse_reset_credit(credit, index))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn optional_reset_credit_values(object: &Map<String, Value>) -> Result<Option<&[Value]>> {
    let Some(value) = object.get("credits") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let credits = value
        .as_array()
        .context("rateLimitResetCredits.credits must be an array or null")?;
    Ok(Some(credits))
}

fn parse_reset_credit(value: &Value, index: usize) -> Result<RateLimitResetCredit> {
    let path = format!("rateLimitResetCredits.credits[{index}]");
    let object = value
        .as_object()
        .with_context(|| format!("{path} must be an object"))?;

    match object.get("id") {
        Some(Value::String(_)) => {}
        _ => bail!("{path}.id is required and must be a string"),
    }

    let granted_at = optional_unix_seconds_timestamp(object, "grantedAt")
        .with_context(|| format!("{path}.grantedAt is invalid"))?
        .with_context(|| format!("{path}.grantedAt is required"))?;
    let expires_at = optional_unix_seconds_timestamp(object, "expiresAt")
        .with_context(|| format!("{path}.expiresAt is invalid"))?;
    let status =
        required_string(object, "status").with_context(|| format!("{path}.status is invalid"))?;
    let reset_type = required_string(object, "resetType")
        .with_context(|| format!("{path}.resetType is invalid"))?;

    Ok(RateLimitResetCredit {
        granted_at,
        expires_at,
        status,
        reset_type,
        title: optional_string(object, "title")
            .with_context(|| format!("{path}.title is invalid"))?,
        description: optional_string(object, "description")
            .with_context(|| format!("{path}.description is invalid"))?,
    })
}

fn optional_unix_seconds_timestamp(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<DateTime<Utc>>> {
    match object.get(key) {
        Some(Value::Number(value)) => {
            let seconds = value
                .as_i64()
                .ok_or_else(|| anyhow!("{key} must be Unix seconds as an int64"))?;
            DateTime::from_timestamp(seconds, 0)
                .map(Some)
                .ok_or_else(|| anyhow!("{key} is outside the supported timestamp range"))
        }
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{key} must be Unix seconds as an integer or null"),
    }
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
        return compact_diagnostic_text(&error.to_string(), RPC_ERROR_MESSAGE_LIMIT);
    };
    let message = compact_diagnostic_text(
        object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error"),
        RPC_ERROR_MESSAGE_LIMIT,
    );
    match object.get("code") {
        Some(code) => format!("{message} (code {code})"),
        None => message,
    }
}

fn optional_usage_rpc_unavailable(message: &str) -> bool {
    message.contains("(code -32601)")
        || (message.contains("(code -32600)")
            && message.contains("account/usage/read")
            && message.to_ascii_lowercase().contains("unknown variant"))
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
                    *destination = compact_diagnostic_text(
                        &String::from_utf8_lossy(&captured),
                        STDERR_DIAGNOSTIC_LIMIT,
                    );
                }
            }
        }
    }
}

fn compact_diagnostic_text(value: &str, max_chars: usize) -> String {
    let stripped = strip_ansi_sequences(value);
    let normalized = stripped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut compact = normalized
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    compact.push_str("...");
    compact
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            if !character.is_control() || matches!(character, '\n' | '\t') {
                output.push(character);
            }
            continue;
        }

        match chars.next() {
            Some('[') => {
                for character in chars.by_ref() {
                    if ('@'..='~').contains(&character) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut previous_escape = false;
                for character in chars.by_ref() {
                    if character == '\u{7}' || (previous_escape && character == '\\') {
                        break;
                    }
                    previous_escape = character == '\u{1b}';
                }
            }
            Some(_) | None => {}
        }
    }
    output
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

#[cfg(test)]
mod diagnostic_tests {
    use serde_json::json;

    use super::{
        RPC_ERROR_MESSAGE_LIMIT, compact_diagnostic_text, format_rpc_error,
        optional_usage_rpc_unavailable,
    };

    #[test]
    fn unsupported_optional_usage_errors_are_recognized_without_matching_other_failures() {
        assert!(optional_usage_rpc_unavailable(
            "usage disabled (code -32601)"
        ));
        assert!(optional_usage_rpc_unavailable(
            "Invalid request: unknown variant `account/usage/read`, expected one of `initialize` (code -32600)"
        ));
        assert!(!optional_usage_rpc_unavailable(
            "Invalid request: malformed payload (code -32600)"
        ));
    }

    #[test]
    fn external_diagnostics_strip_ansi_and_bound_rpc_messages() {
        assert_eq!(
            compact_diagnostic_text(
                "\u{1b}[2m2026-08-25\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m bad config\n",
                128,
            ),
            "2026-08-25 ERROR bad config"
        );

        let error = format_rpc_error(&json!({
            "code": -32600,
            "message": "x".repeat(RPC_ERROR_MESSAGE_LIMIT + 100)
        }));
        assert!(error.ends_with("... (code -32600)"));
        assert!(error.chars().count() <= RPC_ERROR_MESSAGE_LIMIT + 16);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::{app_server_spawn_error, select_windows_codex_program};

    fn desktop_resource_path() -> PathBuf {
        PathBuf::from(
            r"C:\Program Files\WINDOWSAPPS\openai.codex_26.818.0.0_x64__test\APP\RESOURCES\CODEX.EXE",
        )
    }

    #[test]
    fn desktop_resource_access_denied_gets_an_actionable_hint() {
        let path = desktop_resource_path();
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(path.as_os_str(), io::Error::from_raw_os_error(5))
        );

        assert!(rendered.contains("Codex Desktop packaged resource"));
        assert!(rendered.contains(&path.display().to_string()));
        assert!(rendered.contains("--codex-bin"));
        assert!(rendered.contains("--offline"));
    }

    #[test]
    fn desktop_resource_other_permission_error_keeps_its_original_diagnostic() {
        let path = desktop_resource_path();
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(
                path.as_os_str(),
                io::Error::new(io::ErrorKind::PermissionDenied, "blocked by policy"),
            )
        );

        assert!(rendered.contains("blocked by policy"));
        assert!(!rendered.contains("Codex Desktop packaged resource"));
    }

    #[test]
    fn ordinary_access_denied_is_not_reclassified_as_a_desktop_resource() {
        let path = PathBuf::from(r"C:\tools\codex.exe");
        let rendered = format!(
            "{:#}",
            app_server_spawn_error(path.as_os_str(), io::Error::from_raw_os_error(5))
        );

        assert!(!rendered.contains("Codex Desktop packaged resource"));
    }

    #[test]
    fn installed_windows_cli_replaces_only_the_desktop_packaged_resource() {
        let desktop = desktop_resource_path();
        let installed =
            PathBuf::from(r"C:\Users\developer\AppData\Local\OpenAI\Codex\bin\codex.exe");
        let path_cli = PathBuf::from(r"C:\tools\codex.cmd");

        assert_eq!(
            select_windows_codex_program(Some(desktop.clone()), Some(installed.clone())),
            installed
        );
        assert_eq!(
            select_windows_codex_program(Some(path_cli.clone()), Some(installed.clone())),
            path_cli
        );
        assert_eq!(
            select_windows_codex_program(None, Some(installed.clone())),
            installed
        );
        assert_eq!(
            select_windows_codex_program(Some(desktop.clone()), None),
            desktop
        );
        assert_eq!(
            select_windows_codex_program(None, None),
            PathBuf::from("codex")
        );
    }
}
