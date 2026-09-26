//! Machine service commands bypass the installing administrator's user state.
use super::{FormatArg, ServiceStatusArgs, write_stdout};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Clone, Debug, Args)]
pub(super) struct MachineArgs {
    #[command(subcommand)]
    action: MachineCommand,
}

#[derive(Clone, Debug, Subcommand)]
enum MachineCommand {
    /// Install using a named service account and explicit absolute data paths.
    Install(Box<MachineInstallArgs>),
    Status(MachineOperationArgs),
    /// Start the service without changing its configured startup mode.
    Start(MachineOperationArgs),
    /// Stop the service without changing its configured startup mode.
    Stop(MachineOperationArgs),
    Restart(MachineOperationArgs),
    /// Apply this executable to an existing service, preserving its account and data paths.
    Upgrade(MachineOperationArgs),
    /// Remove the service registration; preserve history and configuration.
    Uninstall(MachineOperationArgs),
    #[command(hide = true)]
    Run {
        #[arg(long)]
        name: String,
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Clone, Debug, Args)]
struct MachineOperationArgs {
    #[arg(long)]
    name: String,
    #[command(flatten)]
    output: ServiceStatusArgs,
}

#[derive(Clone, Debug, Args)]
struct MachineInstallArgs {
    #[arg(long)]
    name: String,
    /// DOMAIN\user or MACHINE\user; built-in service accounts are not supported.
    #[arg(long)]
    account: String,
    /// Read one password line from stdin, never from an argument or environment variable.
    #[arg(long, required = true)]
    password_stdin: bool,
    #[arg(long, value_name = "ABSOLUTE_DIR")]
    codex_home: PathBuf,
    #[arg(long, value_name = "ABSOLUTE_DIR")]
    history_dir: PathBuf,
    #[arg(long, value_name = "ABSOLUTE_FILE")]
    status_file: PathBuf,
    #[arg(long, value_name = "ABSOLUTE_DIR")]
    config_dir: PathBuf,
    #[arg(
        long,
        required_unless_present = "offline",
        value_name = "ABSOLUTE_FILE"
    )]
    codex_bin: Option<PathBuf>,
    #[arg(long)]
    remotes_config_file: Option<PathBuf>,
    #[arg(long)]
    project_mapping_file: Option<PathBuf>,
    /// Explicit PATH for this account; the installing administrator's PATH is not copied.
    #[arg(long)]
    environment_path: Option<String>,
    #[arg(long)]
    offline: bool,
    #[arg(long)]
    redact_content: bool,
    #[arg(long, default_value_t = 7)]
    days: u32,
    #[arg(long, default_value_t = 500)]
    max_files: usize,
    #[arg(long, default_value_t = 5)]
    active_grace_minutes: u64,
    /// Register without starting or enabling automatic startup.
    #[arg(long)]
    disabled: bool,
    #[command(flatten)]
    output: ServiceStatusArgs,
}

pub(super) fn run(args: &MachineArgs, no_rollout_cache: bool) -> Result<i32> {
    use crate::windows_scm::{self, MachineAction, MachineInstallOptions, MachineRecorderOptions};
    let (report, output) = match &args.action {
        MachineCommand::Run { name, config } => return windows_scm::run(name, config),
        MachineCommand::Install(options) => (
            windows_scm::install(
                MachineInstallOptions {
                    name: options.name.clone(),
                    account: options.account.clone(),
                    enabled: !options.disabled,
                    recorder: MachineRecorderOptions {
                        codex_home: options.codex_home.clone(),
                        history_dir: options.history_dir.clone(),
                        status_file: options.status_file.clone(),
                        codex_bin: options.codex_bin.clone(),
                        config_dir: Some(options.config_dir.clone()),
                        remotes_config_file: options.remotes_config_file.clone(),
                        project_mapping_file: options.project_mapping_file.clone(),
                        offline: options.offline,
                        redact_content: options.redact_content,
                        no_rollout_cache,
                        lookback_days: options.days,
                        max_files: options.max_files,
                        active_grace_minutes: options.active_grace_minutes,
                        environment_path: options.environment_path.clone(),
                    },
                },
                options.password_stdin,
            )?,
            &options.output,
        ),
        command => {
            let (options, action) = match command {
                MachineCommand::Status(options) => (options, MachineAction::Status),
                MachineCommand::Start(options) => (options, MachineAction::Start),
                MachineCommand::Stop(options) => (options, MachineAction::Stop),
                MachineCommand::Restart(options) => (options, MachineAction::Restart),
                MachineCommand::Upgrade(options) => (options, MachineAction::Upgrade),
                MachineCommand::Uninstall(options) => (options, MachineAction::Uninstall),
                _ => unreachable!(),
            };
            (
                windows_scm::operate(&options.name, action)?,
                &options.output,
            )
        }
    };
    // The machine report has separate registration and heartbeat state.
    let value = serde_json::to_value(report)?;
    if matches!(output.format, FormatArg::Json) {
        write_stdout(&if output.compact {
            serde_json::to_string(&value)?
        } else {
            serde_json::to_string_pretty(&value)?
        })?;
    } else {
        for (label, field) in [
            ("Machine service", "name"),
            ("Phase", "phase"),
            ("Account", "account"),
            ("State", "managerState"),
            ("Healthy", "healthy"),
            ("Executable", "executable"),
            ("Diagnostic", "diagnostic"),
        ] {
            if let Some(item) = value.get(field).filter(|item| !item.is_null()) {
                write_stdout(&format!(
                    "{label}: {}",
                    item.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| item.to_string())
                ))?;
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn machine_install_requires_explicit_account_and_data_context() {
        assert!(
            super::super::Cli::try_parse_from([
                "codex-usage-monit",
                "service",
                "machine",
                "install",
            ])
            .is_err()
        );
        let arguments = [
            "codex-usage-monit",
            "service",
            "machine",
            "install",
            "--name",
            "MonitServer",
            "--account",
            r"MYPC\recorder",
            "--password-stdin",
            "--codex-home",
            r"C:\Users\recorder\.codex",
            "--history-dir",
            r"C:\Users\recorder\state\history",
            "--status-file",
            r"C:\Users\recorder\state\status.json",
            "--config-dir",
            r"C:\Users\recorder\config",
            "--offline",
            "--no-rollout-cache",
        ];
        let cli = super::super::Cli::try_parse_from(arguments).unwrap();
        assert!(!super::super::command_uses_model_catalog(
            cli.command.as_ref()
        ));
        assert!(cli.no_rollout_cache);
        let Some(super::super::Command::Service(super::super::ServiceArgs {
            action: super::super::ServiceAction::Machine(machine),
        })) = cli.command
        else {
            panic!("machine subcommand was lost")
        };
        let MachineCommand::Install(options) = machine.action else {
            panic!("install subcommand was lost")
        };
        assert_eq!(
            options.codex_home,
            PathBuf::from(r"C:\Users\recorder\.codex")
        );
        assert!(options.codex_bin.is_none());
        assert!(
            super::super::Cli::try_parse_from(
                arguments.into_iter().filter(|arg| *arg != "--offline")
            )
            .is_err(),
            "online collection must select a Codex executable explicitly"
        );
        assert!(
            super::super::Cli::try_parse_from(
                arguments
                    .into_iter()
                    .filter(|arg| *arg != "--password-stdin")
            )
            .is_err(),
            "no ambient service account password or identity"
        );
    }
}
