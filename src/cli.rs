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

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SectionArg {
    Limits,
    Tasks,
    Turns,
    Models,
    Attribution,
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
            SectionArg::Health => Self::Health,
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
        crate::tui::run(config)?;
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
        assert_eq!(Section::all().len(), 6);
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
    fn output_writes_report_broken_pipes_without_panicking() {
        let error = write_output(&mut BrokenPipeWriter, "large output").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn active_grace_conversion_saturates() {
        assert_eq!(active_grace(u64::MAX), Duration::from_secs(u64::MAX));
    }
}
