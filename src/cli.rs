use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::CollectConfig;
use crate::output::{
    OutputFormat, OutputRequest, Section, render_output, request_is_failure, request_is_partial,
};
use crate::snapshot::{collect_limits_snapshot, collect_snapshot};
use crate::tui::Theme;

#[derive(Debug, Parser)]
#[command(
    name = "codex-usage-monit",
    version,
    about = "Monitor local Codex tasks, tokens, and quota windows"
)]
pub struct Cli {
    #[arg(long, value_name = "DIR")]
    codex_home: Option<PathBuf>,

    #[arg(long, default_value_t = 7)]
    days: i64,

    #[arg(long, default_value_t = 500)]
    max_files: usize,

    #[arg(long, default_value_t = 5)]
    active_grace_minutes: u64,

    #[arg(long)]
    offline: bool,

    #[arg(long)]
    redact_content: bool,

    /// TUI color theme; `bright` is an alias for `light`.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print a complete or filtered one-shot snapshot.
    Snapshot(SnapshotArgs),
    /// Print quota windows.
    Limits(OutputArgs),
    /// Print recent tasks.
    Tasks(OutputArgs),
    /// Print turns, optionally restricted to one thread.
    Turns(TurnArgs),
    /// Print model usage for the current quota window.
    Models(OutputArgs),
    /// Print quota attribution and data-quality details.
    Attribution(OutputArgs),
    /// Print per-task, turn, and model usage for each current reset cycle.
    Windows(OutputArgs),
}

#[derive(Clone, Debug, Args)]
struct OutputArgs {
    #[arg(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,

    #[arg(long)]
    compact: bool,
}

#[derive(Clone, Debug, Args)]
struct SnapshotArgs {
    #[command(flatten)]
    output: OutputArgs,

    #[arg(long, value_enum, value_delimiter = ',')]
    section: Vec<SectionArg>,
}

#[derive(Clone, Debug, Args)]
struct TurnArgs {
    #[command(flatten)]
    output: OutputArgs,

    #[arg(long)]
    thread: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ThemeArg {
    Dark,
    #[value(alias = "bright")]
    Light,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SectionArg {
    Limits,
    Tasks,
    Turns,
    Models,
    Attribution,
    Windows,
    Health,
}

impl From<FormatArg> for OutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Text => Self::Text,
            FormatArg::Json => Self::Json,
        }
    }
}

impl From<SectionArg> for Section {
    fn from(value: SectionArg) -> Self {
        match value {
            SectionArg::Limits => Self::Limits,
            SectionArg::Tasks => Self::Tasks,
            SectionArg::Turns => Self::Turns,
            SectionArg::Models => Self::Models,
            SectionArg::Attribution => Self::Attribution,
            SectionArg::Windows => Self::Windows,
            SectionArg::Health => Self::Health,
        }
    }
}

impl From<ThemeArg> for Theme {
    fn from(value: ThemeArg) -> Self {
        match value {
            ThemeArg::Dark => Self::Dark,
            ThemeArg::Light => Self::Light,
        }
    }
}

pub fn run() -> Result<i32> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = if error.use_stderr() { 64 } else { 0 };
            error.print()?;
            return Ok(exit_code);
        }
    };
    run_with(cli)
}

fn run_with(cli: Cli) -> Result<i32> {
    let mut config = CollectConfig::default();
    if let Some(codex_home) = cli.codex_home {
        config.codex_home = codex_home;
    }
    config.lookback_days = cli.days.max(1);
    config.max_files = cli.max_files.max(1);
    config.active_grace = active_grace(cli.active_grace_minutes);
    config.offline = cli.offline;
    config.redact_content = cli.redact_content;

    let Some(command) = cli.command else {
        if let Some(theme) = cli.theme {
            crate::tui::run_with_theme(config, theme.into())?;
        } else {
            crate::tui::run(config)?;
        }
        return Ok(0);
    };

    let request = match command {
        Command::Snapshot(args) => OutputRequest {
            format: args.output.format.into(),
            compact: args.output.compact,
            sections: if args.section.is_empty() {
                Section::all()
            } else {
                args.section.into_iter().map(Into::into).collect()
            },
            thread_filter: None,
        },
        Command::Limits(args) => request_for(args, Section::Limits),
        Command::Tasks(args) => request_for(args, Section::Tasks),
        Command::Turns(args) => OutputRequest {
            format: args.output.format.into(),
            compact: args.output.compact,
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: args.thread,
        },
        Command::Models(args) => request_for(args, Section::Models),
        Command::Attribution(args) => request_for(args, Section::Attribution),
        Command::Windows(args) => request_for(args, Section::Windows),
    };
    let limits_only = request.sections.len() == 1 && request.sections.contains(&Section::Limits);
    let mut result = if limits_only && !config.offline {
        collect_limits_snapshot(&config, None, true)
    } else {
        collect_snapshot(&config, None, true)
    };
    if limits_only && result.snapshot.limits.is_empty() {
        result = collect_snapshot(&config, Some(result.account), false);
    }

    let output = render_output(&result.snapshot, &request)?;
    let mut stdout = io::stdout().lock();
    if let Err(error) = write_output(&mut stdout, &output) {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(0);
        }
        return Err(error.into());
    }

    Ok(if request_is_failure(&result.snapshot, &request) {
        1
    } else if request_is_partial(&result.snapshot, &request) {
        2
    } else {
        0
    })
}

fn write_output(writer: &mut impl Write, output: &str) -> io::Result<()> {
    writer.write_all(output.as_bytes())?;
    writer.write_all(b"\n")
}

fn active_grace(minutes: u64) -> Duration {
    Duration::from_secs(minutes.max(1).saturating_mul(60))
}

fn request_for(args: OutputArgs, section: Section) -> OutputRequest {
    OutputRequest {
        format: args.format.into(),
        compact: args.compact,
        sections: BTreeSet::from([section]),
        thread_filter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn section_all_contains_every_public_section() {
        assert_eq!(Section::all().len(), 7);
    }

    #[test]
    fn clap_errors_map_to_sysexits_usage() {
        let error = Cli::try_parse_from(["codex-usage-monit", "--not-a-real-option"]).unwrap_err();
        assert!(error.use_stderr());
    }

    #[test]
    fn clap_help_is_successful() {
        let error = Cli::try_parse_from(["codex-usage-monit", "--help"]).unwrap_err();
        assert!(!error.use_stderr());
    }

    #[test]
    fn windows_are_available_as_a_command_and_snapshot_section() {
        let command =
            Cli::try_parse_from(["codex-usage-monit", "windows", "--format", "json"]).unwrap();
        assert!(matches!(command.command, Some(Command::Windows(_))));

        let snapshot =
            Cli::try_parse_from(["codex-usage-monit", "snapshot", "--section", "windows"]).unwrap();
        assert!(matches!(
            snapshot.command,
            Some(Command::Snapshot(SnapshotArgs { section, .. }))
                if matches!(section.as_slice(), [SectionArg::Windows])
        ));
    }

    #[test]
    fn tui_theme_uses_saved_default_and_accepts_explicit_light_aliases() {
        let default = Cli::try_parse_from(["codex-usage-monit"]).unwrap();
        assert_eq!(default.theme, None);

        let light = Cli::try_parse_from(["codex-usage-monit", "--theme", "light"]).unwrap();
        assert_eq!(light.theme, Some(ThemeArg::Light));

        let bright = Cli::try_parse_from(["codex-usage-monit", "--theme", "bright"]).unwrap();
        assert_eq!(bright.theme, Some(ThemeArg::Light));
    }

    #[test]
    fn output_writes_report_broken_pipes_without_panicking() {
        let error = write_output(&mut BrokenPipeWriter, "large output").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn active_grace_conversion_saturates() {
        assert_eq!(active_grace(u64::MAX), Duration::from_secs(u64::MAX));
    }
}
