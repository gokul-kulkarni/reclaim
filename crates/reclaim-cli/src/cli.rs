//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use reclaim_core::model::{Group, Tier};

#[derive(Debug, Parser)]
#[command(
    name = "reclaim",
    version,
    about = "Find and safely reclaim disk space taken by developer caches and build artifacts",
    long_about = "reclaim scans the caches and build artifacts left behind by your toolchains \
                  (Node, Python, Rust, JVM, Go, Xcode, Android, Docker and more), shows you how \
                  stale each one is and what it costs to get back, and removes only what you choose.\n\n\
                  Run with no arguments for the interactive terminal UI.",
    after_help = "EXAMPLES:\n  \
        reclaim                              interactive terminal UI\n  \
        reclaim scan                         list what could be reclaimed, delete nothing\n  \
        reclaim scan --json                  machine-readable output\n  \
        reclaim clean --tier safe --yes      remove regenerable caches without prompting\n  \
        reclaim clean --older-than 90d       only things untouched for 90 days\n  \
        reclaim ui                           rich web UI on localhost\n  \
        reclaim schedule install             run weekly in the background"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub global: GlobalArgs,
}

#[derive(Debug, Args, Clone)]
pub struct GlobalArgs {
    /// Use this config file instead of ~/.config/reclaim/config.toml.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Treat this directory as the home directory. Everything is scoped to it.
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Worker threads. 0 means auto.
    #[arg(long, global = true, value_name = "N")]
    pub concurrency: Option<usize>,

    /// Print more detail about what is happening.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Never use colour, even on a terminal.
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scan and report. Never deletes anything.
    Scan(ScanArgs),

    /// Scan, then remove what matches the filters.
    Clean(CleanArgs),

    /// Open the web UI on localhost.
    Ui(UiArgs),

    /// Show what previous runs did.
    History(HistoryArgs),

    /// Inspect and manage the config file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage scheduled background cleanups.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },

    /// List every provider and whether it is enabled.
    Providers,

    /// Print a shell completion script.
    ///
    /// Packagers call this at install time; users rarely need it directly.
    Completions {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
}

/// Filters shared by `scan` and `clean`.
#[derive(Debug, Args, Clone, Default)]
pub struct FilterArgs {
    /// Only this tier and safer. `safe` is the most conservative.
    #[arg(long, value_name = "TIER")]
    pub tier: Option<Tier>,

    /// Only items untouched for at least this long, e.g. 90d, 6w, 3mo.
    #[arg(long, value_name = "AGE", value_parser = parse_days)]
    pub older_than: Option<u32>,

    /// Hide items smaller than this, e.g. 500MB.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Only these ecosystems, e.g. --group node,rust
    #[arg(long, value_name = "GROUPS", value_delimiter = ',')]
    pub group: Vec<Group>,

    /// Only these providers, e.g. --provider node.npm-cache,rust
    #[arg(long, value_name = "IDS", value_delimiter = ',')]
    pub provider: Vec<String>,

    /// Include items modified in the last 24 hours. Excluded by default because
    /// a build or dev server may be writing to them right now.
    #[arg(long)]
    pub include_active: bool,
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,

    /// Show every item, ignoring the configured size threshold.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Report what would be removed, then stop. This is the default unless --yes
    /// is given, so a mistyped filter can never delete anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Proceed without prompting. Caution-tier items still prompt unless
    /// `delete.confirm_caution` is false in the config.
    #[arg(short, long)]
    pub yes: bool,

    /// Move everything to the Trash rather than deleting.
    #[arg(long, conflicts_with = "purge")]
    pub trash: bool,

    /// Delete everything permanently rather than using the Trash.
    #[arg(long, conflicts_with = "trash")]
    pub purge: bool,

    /// Emit a JSON run record instead of human output.
    #[arg(long)]
    pub json: bool,

    /// Internal: marks a run started by the scheduler. Applies the schedule
    /// section's constraints and records the run as background activity.
    #[arg(long, hide = true)]
    pub scheduled: bool,
}

#[derive(Debug, Args)]
pub struct UiArgs {
    /// Port to listen on. 0 picks a free one.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Do not open a browser automatically.
    #[arg(long)]
    pub no_open: bool,

    /// Proxy the frontend to a Vite dev server instead of the embedded assets.
    #[arg(long, hide = true)]
    pub dev: bool,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// How many runs to show.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub last: usize,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the path to the config file.
    Path,
    /// Write a commented default config file.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective config, after env and defaults are applied.
    Show,
    /// Check the config file for errors.
    Validate,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Install a background job (launchd on macOS, systemd user timer on Linux).
    Install(ScheduleInstallArgs),
    /// Show whether a job is installed and what it last did.
    Status,
    /// Remove the background job.
    Uninstall,
    /// Run the scheduled cleanup once, right now.
    RunNow {
        /// Report only.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct ScheduleInstallArgs {
    /// weekly, biweekly, monthly, or a five-field cron expression.
    #[arg(long, value_name = "WHEN")]
    pub cadence: Option<String>,

    /// Tiers a background run may touch. `caution` is refused.
    #[arg(long, value_name = "TIERS", value_delimiter = ',')]
    pub tier: Vec<Tier>,

    /// Only remove items untouched for at least this long.
    #[arg(long, value_name = "AGE", value_parser = parse_days)]
    pub older_than: Option<u32>,

    /// Overwrite an existing installed job.
    #[arg(long)]
    pub force: bool,
}

fn parse_days(raw: &str) -> Result<u32, String> {
    reclaim_core::format::parse_days(raw)
}

fn parse_size(raw: &str) -> Result<u64, String> {
    reclaim_core::format::parse_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_means_the_interactive_ui() {
        let cli = Cli::parse_from(["reclaim"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn age_filters_accept_human_durations() {
        let cli = Cli::parse_from(["reclaim", "scan", "--older-than", "6w"]);
        let Some(Command::Scan(args)) = cli.command else {
            panic!("expected scan")
        };
        assert_eq!(args.filter.older_than, Some(42));
    }

    #[test]
    fn size_filters_accept_human_sizes() {
        let cli = Cli::parse_from(["reclaim", "scan", "--min-size", "500MB"]);
        let Some(Command::Scan(args)) = cli.command else {
            panic!("expected scan")
        };
        assert_eq!(args.filter.min_size, Some(500 * 1024 * 1024));
    }

    #[test]
    fn an_unparseable_duration_is_rejected_at_the_boundary() {
        assert!(Cli::try_parse_from(["reclaim", "scan", "--older-than", "soon"]).is_err());
        assert!(Cli::try_parse_from(["reclaim", "scan", "--min-size", "huge"]).is_err());
    }

    #[test]
    fn groups_and_providers_split_on_commas() {
        let cli = Cli::parse_from([
            "reclaim",
            "clean",
            "--group",
            "node,rust",
            "--provider",
            "jvm.gradle-caches,go",
        ]);
        let Some(Command::Clean(args)) = cli.command else {
            panic!("expected clean")
        };
        assert_eq!(args.filter.group, vec![Group::Node, Group::Rust]);
        assert_eq!(args.filter.provider, vec!["jvm.gradle-caches", "go"]);
    }

    #[test]
    fn trash_and_purge_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["reclaim", "clean", "--trash", "--purge"]).is_err());
    }

    #[test]
    fn tiers_parse_by_name() {
        let cli = Cli::parse_from(["reclaim", "clean", "--tier", "review"]);
        let Some(Command::Clean(args)) = cli.command else {
            panic!("expected clean")
        };
        assert_eq!(args.filter.tier, Some(Tier::Review));
        assert!(Cli::try_parse_from(["reclaim", "clean", "--tier", "reckless"]).is_err());
    }

    #[test]
    fn completions_are_generated_for_every_supported_shell() {
        // The Homebrew formula shells out to this at install time, so a change
        // that breaks it silently breaks packaging.
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ] {
            let mut buffer = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "reclaim", &mut buffer);
            let script = String::from_utf8(buffer).unwrap();
            assert!(script.contains("reclaim"), "empty completion for {shell}");
            assert!(script.contains("scan"), "subcommands missing for {shell}");
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::parse_from(["reclaim", "scan", "--verbose", "--no-color"]);
        assert_eq!(cli.global.verbose, 1);
        assert!(cli.global.no_color);
    }
}
