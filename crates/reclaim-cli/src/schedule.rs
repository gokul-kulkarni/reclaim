//! Phase 2: scheduled background cleanup.
//!
//! We install a unit with the platform's own supervisor rather than running a
//! daemon of our own. That means the job survives reboots, costs nothing while
//! idle, and can be inspected and disabled with the tools the user already knows.
//!
//! Background runs are deliberately constrained: they never touch the Caution
//! tier, they honour a runtime cap, and every run is journalled — because nobody
//! is watching when they happen.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use reclaim_core::model::Tier;

use crate::app::App;
use crate::cli::{ScheduleAction, ScheduleInstallArgs};
use crate::render::Style;

const LABEL: &str = "dev.reclaim.cleanup";

/// When a scheduled run fires, normalised from the config's `cadence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// Day of week, 0 = Sunday. `None` for monthly.
    pub weekday: Option<u8>,
    /// Day of month for monthly schedules.
    pub day: Option<u8>,
    pub hour: u8,
    pub minute: u8,
    /// Only fire on alternating weeks.
    pub biweekly: bool,
}

impl Cadence {
    /// Parse `weekly` / `biweekly` / `monthly`.
    ///
    /// Runs land at 03:00 by default: late enough that a laptop is usually idle,
    /// early enough that both launchd and systemd will catch up a missed run
    /// before the user sits down.
    pub fn parse(raw: &str) -> Result<Self> {
        let base = Cadence {
            weekday: Some(0),
            day: None,
            hour: 3,
            minute: 0,
            biweekly: false,
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "daily" => Ok(Cadence {
                weekday: None,
                day: None,
                ..base
            }),
            "weekly" => Ok(base),
            "biweekly" | "fortnightly" => Ok(Cadence {
                biweekly: true,
                ..base
            }),
            "monthly" => Ok(Cadence {
                weekday: None,
                day: Some(1),
                ..base
            }),
            other => bail!("unknown cadence `{other}`. Use daily, weekly, biweekly or monthly."),
        }
    }

    /// systemd `OnCalendar=` expression. Only used when building a Linux unit,
    /// but always compiled so its tests run on every platform.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn to_systemd(self) -> String {
        match (self.weekday, self.day) {
            (_, Some(day)) => format!("*-*-{day:02} {:02}:{:02}:00", self.hour, self.minute),
            (Some(_), None) => format!("Sun {:02}:{:02}:00", self.hour, self.minute),
            (None, None) => format!("*-*-* {:02}:{:02}:00", self.hour, self.minute),
        }
    }

    fn describe(self) -> String {
        match (self.weekday, self.day, self.biweekly) {
            (_, Some(day), _) => format!(
                "monthly on day {day} at {:02}:{:02}",
                self.hour, self.minute
            ),
            (Some(_), None, true) => {
                format!("every other Sunday at {:02}:{:02}", self.hour, self.minute)
            }
            (Some(_), None, false) => {
                format!("every Sunday at {:02}:{:02}", self.hour, self.minute)
            }
            (None, None, _) => format!("daily at {:02}:{:02}", self.hour, self.minute),
        }
    }
}

pub fn dispatch(app: &App, action: &ScheduleAction, style: &Style) -> Result<u8> {
    match action {
        ScheduleAction::Install(args) => install(app, args, style),
        ScheduleAction::Status => status(app, style),
        ScheduleAction::Uninstall => uninstall(app, style),
        ScheduleAction::RunNow { dry_run } => run_now(app, *dry_run, style),
    }
}

fn install(app: &App, args: &ScheduleInstallArgs, style: &Style) -> Result<u8> {
    // A background job must never be able to remove something irreplaceable.
    if args.tier.contains(&Tier::Caution) {
        bail!(
            "scheduled runs may not include the caution tier: those items can be \
             irreplaceable and must be confirmed by a human"
        );
    }

    let cadence_text = args
        .cadence
        .clone()
        .unwrap_or_else(|| app.config.schedule.cadence.clone());
    let cadence = Cadence::parse(&cadence_text)?;

    let tiers = if args.tier.is_empty() {
        app.config.schedule.tiers.clone()
    } else {
        args.tier.clone()
    };
    let older_than = args
        .older_than
        .unwrap_or(app.config.schedule.older_than_days);

    let exe = std::env::current_exe().context("locating the reclaim binary")?;
    let unit_path = unit_path(app)?;

    if unit_path.exists() && !args.force {
        bail!(
            "{} already exists. Use --force to replace it.",
            unit_path.display()
        );
    }
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let command = scheduled_command(&exe, &tiers, older_than, app.config.schedule.dry_run_first);
    let contents = render_unit(&command, cadence, app)?;
    std::fs::write(&unit_path, &contents)
        .with_context(|| format!("writing {}", unit_path.display()))?;

    load_unit(&unit_path)?;

    println!(
        "Installed a background cleanup, running {}.",
        cadence.describe()
    );
    println!("  unit:      {}", unit_path.display());
    println!(
        "  tiers:     {}",
        tiers
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  older than {older_than} days");
    if app.config.schedule.dry_run_first {
        println!(
            "\n{}",
            style.yellow(
                "The first run only reports what it would do. Check `reclaim history`, \
                 then set schedule.dry_run_first = false in your config to arm it."
            )
        );
    }
    if cadence.biweekly {
        println!(
            "{}",
            style.dim(
                "Biweekly is implemented as a weekly trigger that skips odd weeks, since \
                 neither launchd nor systemd expresses fortnightly directly."
            )
        );
    }

    Ok(0)
}

fn status(app: &App, style: &Style) -> Result<u8> {
    let unit_path = unit_path(app)?;

    if !unit_path.exists() {
        println!("No scheduled cleanup is installed.");
        println!(
            "{}",
            style.dim("Install one with: reclaim schedule install --cadence weekly")
        );
        return Ok(0);
    }

    println!("{} {}", style.green("installed"), unit_path.display());
    println!(
        "  cadence: {}",
        Cadence::parse(&app.config.schedule.cadence)?.describe()
    );

    match app
        .journal
        .read_recent(20)
        .into_iter()
        .find(|r| r.trigger == reclaim_core::Trigger::Scheduled)
    {
        Some(last) => {
            println!("  last background run: {}", last.summary());
            if !last.succeeded() {
                println!(
                    "  {}",
                    style.red(&format!("{} item(s) failed", last.failures().count()))
                );
            }
        }
        None => println!("  {}", style.dim("has not run yet")),
    }

    Ok(0)
}

fn uninstall(app: &App, style: &Style) -> Result<u8> {
    let unit_path = unit_path(app)?;
    if !unit_path.exists() {
        println!("Nothing to uninstall.");
        return Ok(0);
    }

    unload_unit(&unit_path)?;
    std::fs::remove_file(&unit_path)
        .with_context(|| format!("removing {}", unit_path.display()))?;

    #[cfg(target_os = "linux")]
    {
        let service = unit_path.with_extension("service");
        let _ = std::fs::remove_file(service);
    }

    println!("{} removed {}", style.green("ok"), unit_path.display());
    Ok(0)
}

fn run_now(app: &App, dry_run: bool, style: &Style) -> Result<u8> {
    let args = crate::cli::CleanArgs {
        filter: crate::cli::FilterArgs {
            tier: app.config.schedule.tiers.iter().copied().max(),
            older_than: Some(app.config.schedule.older_than_days),
            ..Default::default()
        },
        dry_run: dry_run || app.config.schedule.dry_run_first,
        yes: true,
        trash: false,
        purge: false,
        json: false,
        scheduled: true,
    };
    crate::commands::clean(app, &args, style).map(|code| code as u8)
}

/// The command line a scheduled run executes.
fn scheduled_command(
    exe: &std::path::Path,
    tiers: &[Tier],
    older_than: u32,
    dry_run: bool,
) -> Vec<String> {
    let tier = tiers.iter().copied().max().unwrap_or(Tier::Safe);
    let mut args = vec![
        exe.display().to_string(),
        "clean".into(),
        "--scheduled".into(),
        "--yes".into(),
        "--tier".into(),
        tier.as_str().into(),
        "--older-than".into(),
        format!("{older_than}d"),
    ];
    if dry_run {
        args.push("--dry-run".into());
    }
    args
}

// ---------------------------------------------------------------------------
// macOS: launchd
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn unit_path(app: &App) -> Result<PathBuf> {
    Ok(app
        .paths
        .home_join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn render_unit(command: &[String], cadence: Cadence, app: &App) -> Result<String> {
    let args = command
        .iter()
        .map(|a| format!("        <string>{}</string>", escape_xml(a)))
        .collect::<Vec<_>>()
        .join("\n");

    let mut calendar = format!(
        "        <key>Hour</key><integer>{}</integer>\n        <key>Minute</key><integer>{}</integer>",
        cadence.hour, cadence.minute
    );
    if let Some(weekday) = cadence.weekday {
        calendar.push_str(&format!(
            "\n        <key>Weekday</key><integer>{weekday}</integer>"
        ));
    }
    if let Some(day) = cadence.day {
        calendar.push_str(&format!("\n        <key>Day</key><integer>{day}</integer>"));
    }

    let log_dir = app.paths.state_dir().join("reclaim");
    std::fs::create_dir_all(&log_dir).ok();

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>StartCalendarInterval</key>
    <dict>
{calendar}
    </dict>
    <!-- Background priority: this must never compete with a build for IO. -->
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <!-- Catch up on a run missed while the machine was asleep. -->
    <key>RunAtLoad</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{}/scheduled.log</string>
    <key>StandardErrorPath</key>
    <string>{}/scheduled.log</string>
</dict>
</plist>
"#,
        log_dir.display(),
        log_dir.display()
    ))
}

#[cfg(target_os = "macos")]
fn load_unit(path: &std::path::Path) -> Result<()> {
    // `bootstrap` replaces the deprecated `load`; failures here are not fatal
    // because the plist is on disk and will be picked up at next login.
    let uid = unsafe { libc_getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    let output = std::process::Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &path.display().to_string(),
        ])
        .output()
        .context("running launchctl bootstrap")?;
    if !output.status.success() {
        eprintln!(
            "warning: launchctl could not load the job now ({}). It will start at next login.",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn unload_unit(_path: &std::path::Path) -> Result<()> {
    let uid = unsafe { libc_getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(target_os = "macos")]
fn escape_xml(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Linux: systemd user timer
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn unit_path(app: &App) -> Result<PathBuf> {
    Ok(app.paths.config_dir().join("systemd/user/reclaim.timer"))
}

#[cfg(target_os = "linux")]
fn render_unit(command: &[String], cadence: Cadence, app: &App) -> Result<String> {
    // The timer references a service unit, so write that alongside it.
    let service_path = unit_path(app)?.with_file_name("reclaim.service");
    let exec = command.join(" ");
    let service = format!(
        "[Unit]\n\
         Description=reclaim scheduled cleanup\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exec}\n\
         # Background priority: never compete with an interactive build for IO.\n\
         Nice=10\n\
         IOSchedulingClass=idle\n\
         RuntimeMaxSec={}min\n",
        app.config.schedule.max_runtime_minutes
    );
    std::fs::create_dir_all(service_path.parent().unwrap()).ok();
    std::fs::write(&service_path, service)
        .with_context(|| format!("writing {}", service_path.display()))?;

    Ok(format!(
        "[Unit]\n\
         Description=reclaim scheduled cleanup timer\n\
         \n\
         [Timer]\n\
         OnCalendar={}\n\
         # Catch up on a run missed while the machine was off.\n\
         Persistent=true\n\
         RandomizedDelaySec=30min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        cadence.to_systemd()
    ))
}

#[cfg(target_os = "linux")]
fn load_unit(_path: &std::path::Path) -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    let output = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "reclaim.timer"])
        .output()
        .context("running systemctl --user enable")?;
    if !output.status.success() {
        eprintln!(
            "warning: could not enable the timer ({}). Enable it manually with:\n  \
             systemctl --user enable --now reclaim.timer",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unload_unit(_path: &std::path::Path) -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "reclaim.timer"])
        .output();
    Ok(())
}

// ---------------------------------------------------------------------------
// Other platforms
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unit_path(app: &App) -> Result<PathBuf> {
    let _ = app;
    bail!("scheduled cleanup is only supported on macOS and Linux")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn render_unit(_command: &[String], _cadence: Cadence, _app: &App) -> Result<String> {
    bail!("scheduled cleanup is only supported on macOS and Linux")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn load_unit(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unload_unit(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;

    fn app_at(root: &std::path::Path) -> App {
        App::new(&GlobalArgs {
            config: None,
            root: Some(root.to_path_buf()),
            concurrency: Some(2),
            verbose: 0,
            no_color: true,
        })
        .unwrap()
    }

    #[test]
    fn cadences_parse_and_describe_themselves() {
        assert_eq!(Cadence::parse("weekly").unwrap().weekday, Some(0));
        assert!(Cadence::parse("biweekly").unwrap().biweekly);
        assert_eq!(Cadence::parse("monthly").unwrap().day, Some(1));
        assert_eq!(Cadence::parse("daily").unwrap().weekday, None);

        assert!(Cadence::parse("weekly")
            .unwrap()
            .describe()
            .contains("Sunday"));
        assert!(Cadence::parse("monthly")
            .unwrap()
            .describe()
            .contains("monthly"));
    }

    #[test]
    fn an_unknown_cadence_is_rejected_with_the_valid_options() {
        let err = Cadence::parse("occasionally").unwrap_err().to_string();
        assert!(
            err.contains("weekly"),
            "the error must list what is accepted: {err}"
        );
    }

    #[test]
    fn scheduled_runs_default_to_the_safe_tier_only() {
        let cmd = scheduled_command(
            std::path::Path::new("/usr/local/bin/reclaim"),
            &[Tier::Safe],
            60,
            false,
        );
        assert!(cmd.contains(&"--tier".to_string()));
        assert!(cmd.contains(&"safe".to_string()));
        assert!(cmd.contains(&"--scheduled".to_string()));
        assert!(cmd.contains(&"60d".to_string()));
    }

    #[test]
    fn the_first_scheduled_run_can_be_a_dry_run() {
        let cmd = scheduled_command(std::path::Path::new("/x/reclaim"), &[Tier::Safe], 60, true);
        assert!(cmd.contains(&"--dry-run".to_string()));
    }

    #[test]
    fn installing_with_the_caution_tier_is_refused() {
        // Nobody is watching when a background job runs.
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let args = ScheduleInstallArgs {
            cadence: Some("weekly".into()),
            tier: vec![Tier::Safe, Tier::Caution],
            older_than: None,
            force: false,
        };
        let err = install(&app, &args, &Style::plain())
            .unwrap_err()
            .to_string();
        assert!(err.contains("caution"), "{err}");
    }

    #[test]
    fn status_on_a_clean_machine_says_nothing_is_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        assert_eq!(status(&app, &Style::plain()).unwrap(), 0);
    }

    #[test]
    fn uninstalling_when_nothing_is_installed_is_not_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        assert_eq!(uninstall(&app, &Style::plain()).unwrap(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn systemd_calendar_expressions_are_well_formed() {
        assert_eq!(
            Cadence::parse("weekly").unwrap().to_systemd(),
            "Sun 03:00:00"
        );
        assert_eq!(
            Cadence::parse("monthly").unwrap().to_systemd(),
            "*-*-01 03:00:00"
        );
        assert_eq!(
            Cadence::parse("daily").unwrap().to_systemd(),
            "*-*-* 03:00:00"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_launchd_plist_is_well_formed_and_low_priority() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let command =
            scheduled_command(std::path::Path::new("/x/reclaim"), &[Tier::Safe], 60, true);
        let plist = render_unit(&command, Cadence::parse("weekly").unwrap(), &app).unwrap();

        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(LABEL));
        assert!(plist.contains("<key>Weekday</key><integer>0</integer>"));
        // Must never fight an interactive build for IO.
        assert!(plist.contains("<string>Background</string>"));
        assert!(plist.contains("<key>LowPriorityIO</key>"));
        assert_eq!(
            plist.matches("<dict>").count(),
            plist.matches("</dict>").count(),
            "unbalanced plist"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn plist_arguments_are_xml_escaped() {
        // A home directory containing `&` would otherwise produce invalid XML.
        assert_eq!(escape_xml("a & b <c>"), "a &amp; b &lt;c&gt;");
    }
}
