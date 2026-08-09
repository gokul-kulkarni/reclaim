//! Subcommand implementations for the non-interactive paths.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use reclaim_core::config::{Config, DeleteMode, DEFAULT_CONFIG_TEMPLATE};
use reclaim_core::exec::{self, CleanOptions};
use reclaim_core::journal::Trigger;
use reclaim_core::model::Candidate;
use reclaim_core::report::HistoryReport;

use crate::app::{App, Purpose};
use crate::cli::{CleanArgs, ConfigAction, HistoryAction, HistoryArgs, HistoryReportArgs, ScanArgs};
use crate::render::{self, Style};

pub fn scan(app: &App, args: &ScanArgs, style: &Style) -> Result<()> {
    let filter = app.filter(
        &args.filter,
        if args.all {
            Purpose::ShowEverything
        } else {
            Purpose::Report
        },
    )?;
    let result = app.scan(&filter);

    if args.json {
        // `all` rather than `candidates`, plus the filter verdict, so a script can
        // apply its own policy without rerunning the scan.
        let payload = serde_json::json!({
            "total_reclaimable": result.total_reclaimable(),
            "projects_scanned": result.projects_scanned,
            "elapsed_ms": result.elapsed_ms,
            "unreadable": result.unreadable,
            "candidates": result.candidates,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    print!("{}", render::scan_report(&result, &app.paths, style));
    Ok(())
}

pub fn clean(app: &App, args: &CleanArgs, style: &Style) -> Result<i32> {
    let tier = app.effective_tier(args.filter.tier);
    let mut filter_args = args.filter.clone();
    filter_args.tier = Some(tier);

    let filter = app.filter(&filter_args, Purpose::Delete)?;
    let result = app.scan(&filter);
    let chosen: Vec<Candidate> = result.candidates.clone();

    if chosen.is_empty() {
        println!("Nothing matches those filters. Nothing to do.");
        return Ok(0);
    }

    // A run is a dry run unless the user explicitly consented, so a mistyped
    // filter can report but never delete.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let dry_run = args.dry_run || (!args.yes && !interactive);

    if !dry_run && !args.yes {
        println!(
            "About to reclaim {}:",
            reclaim_core::format::bytes(chosen.iter().map(Candidate::reclaimable).sum::<u64>())
        );
        print!("{}", render::clean_preview(&chosen, &app.paths, style));
        if !confirm("Proceed?")? {
            println!("Cancelled. Nothing was removed.");
            return Ok(0);
        }
    }

    // Caution-tier items get their own confirmation even under --yes, because
    // "yes, clean up my caches" is not consent to delete something irreplaceable.
    let needs_confirmation =
        exec::needs_explicit_confirmation(&chosen, app.config.delete.confirm_caution);
    let chosen = if !dry_run && !needs_confirmation.is_empty() {
        confirm_caution_items(&chosen, &needs_confirmation, app, style, interactive)?
    } else {
        chosen
    };

    if chosen.is_empty() {
        println!("Nothing left to do after confirmations.");
        return Ok(0);
    }

    let options = CleanOptions {
        dry_run,
        mode: delete_mode(app, args),
        trigger: if args.scheduled {
            Trigger::Scheduled
        } else {
            Trigger::Cli
        },
        concurrency: app.config.scan.threads(),
    };

    let record = exec::clean(&chosen, &app.guard, &options, None);

    if let Err(e) = app.journal.write(&record) {
        // Failing to journal must not make a successful cleanup look failed.
        eprintln!(
            "{}",
            style.yellow(&format!("warning: could not write run history: {e}"))
        );
    }
    let _ = app.journal.prune(200);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        print!("{}", render::run_report(&record, style));
        if dry_run {
            println!(
                "\n{}",
                style.dim("This was a dry run. Re-run with --yes to apply.")
            );
        }
    }

    Ok(if record.succeeded() { 0 } else { 1 })
}

/// Ask about each Caution item individually, returning the surviving selection.
fn confirm_caution_items(
    chosen: &[Candidate],
    needs_confirmation: &[&Candidate],
    app: &App,
    style: &Style,
    interactive: bool,
) -> Result<Vec<Candidate>> {
    if !interactive {
        // Never delete an irreplaceable item without a human answering, so in a
        // pipe or a cron job they are dropped rather than silently removed.
        let ids: std::collections::HashSet<_> =
            needs_confirmation.iter().map(|c| c.id.clone()).collect();
        eprintln!(
            "{}",
            style.yellow(&format!(
                "Skipping {} caution-tier item(s): they require an interactive confirmation.",
                ids.len()
            ))
        );
        return Ok(chosen
            .iter()
            .filter(|c| !ids.contains(&c.id))
            .cloned()
            .collect());
    }

    let mut keep = Vec::new();
    for candidate in chosen {
        if candidate.tier != reclaim_core::Tier::Caution {
            keep.push(candidate.clone());
            continue;
        }

        println!();
        println!(
            "  {} {} ({})",
            style.red("CAUTION"),
            style.bold(&candidate.label),
            reclaim_core::format::bytes(candidate.reclaimable())
        );
        for path in &candidate.paths {
            println!("    {}", style.dim(&app.paths.contract(path)));
        }
        for warning in &candidate.warnings {
            println!("    {}", style.severity(warning.severity, &warning.message));
        }
        println!(
            "    {} {}",
            style.dim("comes back:"),
            style.dim(&candidate.regen.summary())
        );

        if confirm("  Remove this?")? {
            keep.push(candidate.clone());
        }
    }
    Ok(keep)
}

fn delete_mode(app: &App, args: &CleanArgs) -> DeleteMode {
    if args.trash {
        DeleteMode::Trash
    } else if args.purge {
        DeleteMode::Purge
    } else {
        app.config.delete.mode
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub fn history(app: &App, args: &HistoryArgs, style: &Style) -> Result<()> {
    if let Some(HistoryAction::Report(report_args)) = &args.action {
        return history_report(app, report_args);
    }

    let records = app.journal.read_recent(args.last);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else {
        print!("{}", render::history_report(&records, style));
    }
    Ok(())
}

fn history_report(app: &App, args: &HistoryReportArgs) -> Result<()> {
    // 0 means "every run"; read_recent's own `0` would mean "none", so this is
    // resolved here rather than teaching the journal a second meaning for 0.
    let limit = if args.last == 0 { usize::MAX } else { args.last };
    let records = app.journal.read_recent(limit);

    let report = HistoryReport::build(&records);
    let html = crate::report_html::render(&report, reclaim_core::VERSION);

    let out_path = args
        .out
        .clone()
        .unwrap_or_else(|| app.journal.dir().join("report.html"));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&out_path, html).with_context(|| format!("writing {}", out_path.display()))?;

    println!(
        "Wrote {} ({} run(s), {} lifetime freed).",
        out_path.display(),
        report.runs,
        reclaim_core::format::bytes(report.lifetime_freed)
    );

    if args.open {
        reclaim_web::open_browser(&format!("file://{}", out_path.display()));
    }

    Ok(())
}

pub fn config(app: &App, action: &ConfigAction, style: &Style) -> Result<()> {
    let path = app.paths.config_file();

    match action {
        ConfigAction::Path => println!("{}", path.display()),

        ConfigAction::Init { force } => {
            if path.exists() && !force {
                bail!(
                    "{} already exists. Use --force to overwrite it.",
                    path.display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("Wrote {}", path.display());
        }

        ConfigAction::Show => print!("{}", app.config.to_toml()?),

        ConfigAction::Validate => {
            // Re-read from disk rather than trusting the already-loaded copy, so
            // this actually validates the file the user just edited.
            let loaded = Config::load_from(&path, &app.paths)
                .with_context(|| format!("validating {}", path.display()))?;
            loaded.validate(&app.paths)?;
            println!("{} {}", style.green("ok"), path.display());
        }
    }

    Ok(())
}

pub fn providers(app: &App, style: &Style) -> Result<()> {
    println!(
        "{}",
        style.bold(&format!(
            "{:<28} {:<10} {}",
            "PROVIDER", "STATUS", "MARKERS"
        ))
    );
    for provider in &app.providers {
        let id = provider.id();
        let enabled = app.config.providers.is_enabled(id);
        let status = if enabled {
            style.green("enabled")
        } else {
            style.dim("disabled")
        };
        let markers = provider.markers().join(", ");
        println!("{id:<28} {status:<10} {}", style.dim(&markers));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{FilterArgs, GlobalArgs};

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

    fn clean_args() -> CleanArgs {
        CleanArgs {
            filter: FilterArgs {
                min_size: Some(0),
                ..Default::default()
            },
            dry_run: true,
            yes: false,
            trash: false,
            purge: false,
            json: false,
            scheduled: false,
        }
    }

    #[test]
    fn delete_mode_flags_override_the_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());

        assert_eq!(delete_mode(&app, &clean_args()), DeleteMode::Tiered);
        assert_eq!(
            delete_mode(
                &app,
                &CleanArgs {
                    trash: true,
                    ..clean_args()
                }
            ),
            DeleteMode::Trash
        );
        assert_eq!(
            delete_mode(
                &app,
                &CleanArgs {
                    purge: true,
                    ..clean_args()
                }
            ),
            DeleteMode::Purge
        );
    }

    #[test]
    fn config_init_writes_a_file_that_parses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());

        config(&app, &ConfigAction::Init { force: false }, &Style::plain()).unwrap();
        let path = app.paths.config_file();
        assert!(path.is_file());

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: Config = toml::from_str(&text).expect("the file we write must be valid");
        parsed.validate(&app.paths).unwrap();
    }

    #[test]
    fn config_init_refuses_to_clobber_without_force() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());

        config(&app, &ConfigAction::Init { force: false }, &Style::plain()).unwrap();
        let err = config(&app, &ConfigAction::Init { force: false }, &Style::plain()).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");

        config(&app, &ConfigAction::Init { force: true }, &Style::plain()).unwrap();
    }

    #[test]
    fn cleaning_an_empty_home_succeeds_and_deletes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let code = clean(&app, &clean_args(), &Style::plain()).unwrap();
        assert_eq!(code, 0);
    }
}
