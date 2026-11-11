//! `reclaim apps`: every installed application, its size, and when it was last
//! opened. Read-only; unused apps are removed from `reclaim`, `scan`/`clean` or
//! the web UI, where they appear in the "Unused apps" group.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};

use anyhow::Result;
use reclaim_core::apps::{self, InstalledApp};
use reclaim_core::format::{bytes, ellipsize, epoch_millis};
use reclaim_core::model::{days_since, humanize_age};
use reclaim_core::Paths;

use crate::app::App;
use crate::cli::{AppSort, AppsArgs};
use crate::render::Style;

/// One row of the listing: an app and its measured size.
#[derive(Debug, Clone)]
struct Row {
    app: InstalledApp,
    on_disk: u64,
}

pub fn run(app: &App, args: &AppsArgs, style: &Style) -> Result<u8> {
    let dirs = app.paths.app_dirs();
    if dirs.is_empty() {
        println!("Listing installed apps is only supported on macOS.");
        return Ok(0);
    }

    let rows = sorted(filtered(collect(dirs), args.unused), args.sort);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json_payload(&rows))?);
        return Ok(0);
    }
    let scan_days = app.config.providers.apps.unused_days;
    print!(
        "{}",
        table(&rows, &app.paths, args.unused, scan_days, style)
    );
    Ok(0)
}

/// List and measure, with a self-erasing status line while bundles are sized.
fn collect(dirs: &[std::path::PathBuf]) -> Vec<Row> {
    let show = std::io::stderr().is_terminal();
    if show {
        eprint!("reclaim: reading installed apps…");
        let _ = std::io::stderr().flush();
    }
    let found = apps::list_apps(dirs, &apps::spotlight_usage);
    let sizes = apps::sizes(&found);
    if show {
        eprint!("\r\x1b[2K");
    }
    found
        .into_iter()
        .zip(sizes)
        .map(|(app, on_disk)| Row { app, on_disk })
        .collect()
}

fn filtered(rows: Vec<Row>, unused_days: Option<u32>) -> Vec<Row> {
    match unused_days {
        None => rows,
        Some(min) => rows
            .into_iter()
            .filter(|r| r.app.idle_days().is_some_and(|d| d >= min))
            .collect(),
    }
}

/// Least recently used first by default, so the forgotten ones lead.
fn sorted(rows: Vec<Row>, by: AppSort) -> Vec<Row> {
    let mut rows = rows;
    match by {
        AppSort::Used => rows.sort_by(|a, b| {
            a.app
                .last_activity()
                .cmp(&b.app.last_activity())
                .then_with(|| b.on_disk.cmp(&a.on_disk))
        }),
        AppSort::Size => rows.sort_by_key(|r| std::cmp::Reverse(r.on_disk)),
        AppSort::Name => rows.sort_by_key(|r| r.app.name.to_lowercase()),
    }
    rows
}

/// "3 months ago", or "never (installed 1 year ago)" without a Spotlight record.
fn last_opened(app: &InstalledApp) -> String {
    match (app.last_opened, app.installed) {
        (Some(opened), _) => humanize_age(days_since(opened)),
        (None, Some(installed)) => {
            format!("never (installed {})", humanize_age(days_since(installed)))
        }
        (None, None) => "unknown".to_string(),
    }
}

fn table(
    rows: &[Row],
    paths: &Paths,
    unused_days: Option<u32>,
    scan_days: u32,
    style: &Style,
) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        let _ = match unused_days {
            Some(days) => writeln!(out, "No apps unopened for {days} days or more."),
            None => writeln!(out, "No apps found."),
        };
        return out;
    }

    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!(
            "{:>9}  {:<32}  {:<28}  {}",
            "SIZE", "LAST OPENED", "APP", "PATH"
        ))
    );
    for row in rows {
        let when = last_opened(&row.app);
        let when = if row.app.last_opened.is_none() {
            style.dim(&format!("{when:<32}"))
        } else {
            format!("{when:<32}")
        };
        let _ = writeln!(
            out,
            "{:>9}  {}  {:<28}  {}",
            bytes(row.on_disk),
            when,
            ellipsize(&row.app.name, 28),
            style.dim(&paths.contract(&row.app.path))
        );
    }

    let total: u64 = rows.iter().map(|r| r.on_disk).sum();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!("{} in {} app(s).", bytes(total), rows.len()))
    );
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "Last opened is the later of Spotlight's record (what Finder shows) and when the \
             app's program was last launched. Apps unopened for {scan_days}+ days also appear \
             in `reclaim` and `reclaim scan` under \"Unused apps\", where they can be moved \
             to the Trash."
        ))
    );
    out
}

/// JSON with timestamps as epoch milliseconds rather than serde's `{secs, nanos}`.
fn json_payload(rows: &[Row]) -> serde_json::Value {
    let apps: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.app.name,
                "path": r.app.path,
                "on_disk": r.on_disk,
                "last_opened_ms": r.app.last_opened.map(epoch_millis),
                "installed_ms": r.app.installed.map(epoch_millis),
                "idle_days": r.app.idle_days(),
                "system_owned": r.app.system_owned,
            })
        })
        .collect();
    serde_json::json!({ "apps": apps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn ago(days: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days * 86_400)
    }

    fn row(name: &str, opened: Option<u64>, installed: u64, on_disk: u64) -> Row {
        Row {
            app: InstalledApp {
                path: PathBuf::from(format!("/Applications/{name}.app")),
                name: name.into(),
                last_opened: opened.map(ago),
                installed: Some(ago(installed)),
                system_owned: false,
            },
            on_disk,
        }
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.app.name.as_str()).collect()
    }

    fn sample() -> Vec<Row> {
        vec![
            row("Daily", Some(0), 900, 10),
            row("Forgotten", Some(300), 900, 20),
            row("Trial", None, 120, 30),
        ]
    }

    #[test]
    fn least_recently_used_comes_first_by_default() {
        let rows = sorted(sample(), AppSort::Used);
        assert_eq!(names(&rows), ["Forgotten", "Trial", "Daily"]);
    }

    #[test]
    fn can_sort_by_size_or_name() {
        assert_eq!(
            names(&sorted(sample(), AppSort::Size)),
            ["Trial", "Forgotten", "Daily"]
        );
        assert_eq!(
            names(&sorted(sample(), AppSort::Name)),
            ["Daily", "Forgotten", "Trial"]
        );
    }

    #[test]
    fn the_unused_filter_counts_never_opened_apps_from_their_install_date() {
        let rows = filtered(sample(), Some(100));
        assert_eq!(names(&rows), ["Forgotten", "Trial"]);
        assert_eq!(filtered(sample(), None).len(), 3);
    }

    #[test]
    fn last_opened_reads_naturally() {
        assert_eq!(last_opened(&row("A", Some(0), 5, 1).app), "today");
        assert_eq!(
            last_opened(&row("B", None, 400, 1).app),
            "never (installed 13 months ago)"
        );
    }

    #[test]
    fn the_table_lists_each_app_and_a_total() {
        let style = Style::plain();
        let out = table(&sample(), &Paths::with_home("/Users/x"), None, 45, &style);
        assert!(out.contains("LAST OPENED"), "{out}");
        assert!(out.contains("Forgotten"), "{out}");
        assert!(out.contains("in 3 app(s)."), "{out}");
        assert!(
            out.contains("45+ days"),
            "the footer follows the config: {out}"
        );

        let none = table(&[], &Paths::with_home("/Users/x"), Some(90), 90, &style);
        assert!(none.contains("90 days"), "{none}");
    }

    #[test]
    fn json_carries_dates_as_epoch_millis() {
        let value = json_payload(&sample());
        let first = &value["apps"][0];
        assert_eq!(first["name"], "Daily");
        assert!(first["last_opened_ms"].as_u64().unwrap() > 0);
        assert!(value["apps"][2]["last_opened_ms"].is_null());
        assert_eq!(value["apps"][1]["idle_days"], 300);
    }
}
