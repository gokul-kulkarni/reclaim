//! How a clean looks from the command line: a live status line while items are
//! removed, then a report grouped by what happened to each item.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};

use reclaim_core::exec::{self, CleanOptions};
use reclaim_core::format::{bytes, ellipsize};
use reclaim_core::journal::{ItemOutcome, RunRecord};
use reclaim_core::model::Candidate;
use reclaim_core::progress::{sections, shown_bytes, CleanProgress, OutcomeSection};
use reclaim_core::{CleanEvent, PathGuard, Paths};

use crate::render::Style;

/// Cells in the text progress bar.
const BAR_WIDTH: usize = 20;
/// Longest item label printed on a line.
const LABEL_WIDTH: usize = 36;
/// Longest path printed on a line.
const PATH_WIDTH: usize = 52;

/// Run a clean, showing progress on stderr when it is a terminal.
///
/// Each finished item is printed as it completes, with a self-erasing line
/// underneath naming what is running now. Nothing goes to stdout, so `--json`
/// stays parseable.
pub fn run_clean(
    chosen: Vec<Candidate>,
    guard: &PathGuard,
    options: CleanOptions,
    paths: &Paths,
    style: &Style,
) -> RunRecord {
    if !std::io::stderr().is_terminal() {
        return exec::clean(&chosen, guard, &options, None);
    }

    let mut progress = CleanProgress::new(options.dry_run);
    let run = exec::spawn_clean(chosen, guard.clone(), options);
    let mut err = std::io::stderr();
    for event in run.events.iter() {
        let finished = match &event {
            CleanEvent::ItemFinished(item) => Some(item_line(item, paths, style)),
            _ => None,
        };
        progress.apply(event);
        let _ = write!(err, "\r\x1b[2K");
        if let Some(line) = finished {
            let _ = writeln!(err, "{line}");
        }
        if !progress.is_finished() {
            let _ = write!(err, "{}", live_line(&progress, style));
        }
        let _ = err.flush();
    }
    let _ = writeln!(err);
    run.join().expect("clean thread panicked")
}

/// `⠿ [######--------------] 3 of 12 · 1.2 GB freed · npm cache`
fn live_line(progress: &CleanProgress, style: &Style) -> String {
    let filled = (progress.ratio() * BAR_WIDTH as f64).round() as usize;
    let bar = format!(
        "[{}{}]",
        "#".repeat(filled),
        "-".repeat(BAR_WIDTH.saturating_sub(filled))
    );
    let verb = if progress.dry_run {
        "checking"
    } else {
        "reclaiming"
    };
    let current = progress
        .current()
        .map(|label| format!(" · {}", ellipsize(label, LABEL_WIDTH)))
        .unwrap_or_default();
    format!(
        "reclaim: {verb} {} {}{}",
        style.cyan(&bar),
        progress.status_line(),
        style.dim(&current)
    )
}

/// One item as it finishes: `  deleted     1.2 GB  npm cache`
fn item_line(item: &ItemOutcome, paths: &Paths, style: &Style) -> String {
    let section = OutcomeSection::of(item.disposition);
    let mark = format!("{:<10}", verb(section));
    let mark = match section {
        OutcomeSection::Failed => style.red(&mark),
        OutcomeSection::Trashed => style.yellow(&mark),
        OutcomeSection::DryRun => style.cyan(&mark),
        OutcomeSection::AlreadyGone => style.dim(&mark),
        OutcomeSection::Removed | OutcomeSection::Command => style.green(&mark),
    };
    format!(
        "  {mark} {:>9}  {}  {}",
        bytes(shown_bytes(item)),
        ellipsize(&item.label, LABEL_WIDTH),
        style.dim(&where_it_was(item, paths))
    )
}

pub(crate) fn verb(section: OutcomeSection) -> &'static str {
    match section {
        OutcomeSection::Failed => "failed",
        OutcomeSection::Removed => "deleted",
        OutcomeSection::Command => "cleaned",
        OutcomeSection::Trashed => "trashed",
        OutcomeSection::DryRun => "would free",
        OutcomeSection::AlreadyGone => "gone",
    }
}

/// The command that ran, or the first path and how many more.
pub(crate) fn where_it_was(item: &ItemOutcome, paths: &Paths) -> String {
    if let Some(command) = &item.command {
        return format!("ran `{command}`");
    }
    match item.paths.as_slice() {
        [] => String::new(),
        [only] => ellipsize(&paths.contract(only), PATH_WIDTH),
        [first, rest @ ..] => format!(
            "{} (+{} more)",
            ellipsize(&paths.contract(first), PATH_WIDTH),
            rest.len()
        ),
    }
}

/// The end-of-run report: items grouped by outcome, failures first, then
/// totals that keep space freed now apart from space sitting in the Trash.
pub fn run_report(record: &RunRecord, paths: &Paths, style: &Style) -> String {
    let mut out = String::new();
    if record.items.is_empty() {
        let _ = writeln!(out, "Nothing was processed.");
        return out;
    }

    for (section, items) in sections(&record.items) {
        let total: u64 = items.iter().map(|i| shown_bytes(i)).sum();
        let heading = format!(
            "{} · {} item(s) · {}",
            section.heading(),
            items.len(),
            bytes(total)
        );
        let heading = match section {
            OutcomeSection::Failed => style.red(&heading),
            _ => style.bold(&heading),
        };
        let _ = writeln!(out, "{heading}");
        let _ = writeln!(
            out,
            "{}",
            style.dim(&format!("  {}", section.explanation()))
        );
        for item in items {
            let _ = writeln!(
                out,
                "  {:>9}  {}",
                bytes(shown_bytes(item)),
                ellipsize(&item.label, LABEL_WIDTH)
            );
            let _ = writeln!(
                out,
                "             {}",
                style.dim(&where_it_was(item, paths))
            );
            if let Some(error) = &item.error {
                let _ = writeln!(out, "             {}", style.red(error));
            }
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "{}", style.bold(&totals_line(record)));
    // Trashed bytes have not actually been returned to the filesystem yet, and
    // saying otherwise would be the most misleading thing this tool could do.
    if record.bytes_trashed() > 0 {
        let _ = writeln!(
            out,
            "{}",
            style.yellow(&format!(
                "{} is in the Trash and still occupies disk until you empty it.",
                bytes(record.bytes_trashed())
            ))
        );
    }
    out
}

pub(crate) fn totals_line(record: &RunRecord) -> String {
    if record.dry_run {
        return format!(
            "Dry run: {}.",
            record.summary().trim_start_matches("dry run: ")
        );
    }
    let mut parts = vec![format!("Freed now: {}", bytes(record.bytes_freed()))];
    if record.bytes_trashed() > 0 {
        parts.push(format!("in the Trash: {}", bytes(record.bytes_trashed())));
    }
    let failed = record.failures().count();
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reclaim_core::journal::{Disposition, Trigger};
    use reclaim_core::model::{CandidateId, Group, Tier};

    fn item(label: &str, disposition: Disposition, bytes: u64) -> ItemOutcome {
        let freed = match disposition {
            Disposition::Purged | Disposition::CommandRun | Disposition::Trashed => bytes,
            _ => 0,
        };
        ItemOutcome {
            id: CandidateId(label.into()),
            provider: "node.npm-cache".into(),
            group: Group::Node,
            label: label.into(),
            tier: Tier::Safe,
            paths: vec!["/home/t/.npm".into()],
            expected_bytes: bytes,
            freed_bytes: freed,
            disposition,
            error: None,
            command: None,
        }
    }

    fn report(items: Vec<ItemOutcome>, dry_run: bool) -> String {
        let mut record = RunRecord::new(Trigger::Cli, dry_run);
        record.items = items;
        run_report(&record, &Paths::with_home("/home/t"), &Style::plain())
    }

    #[test]
    fn the_report_groups_items_by_what_happened_to_them() {
        let text = report(
            vec![
                item("npm cache", Disposition::Purged, 1000),
                item("Xcode Archives", Disposition::Trashed, 5000),
            ],
            false,
        );
        assert!(text.contains("Deleted · 1 item(s)"), "{text}");
        assert!(text.contains("Moved to the Trash · 1 item(s)"), "{text}");
        assert!(
            text.contains("~/.npm"),
            "paths are shown, contracted: {text}"
        );
        assert!(text.contains("Freed now: 1000 B · in the Trash:"), "{text}");
        assert!(
            text.contains("still occupies disk"),
            "trashed bytes must not be presented as freed: {text}"
        );
    }

    #[test]
    fn failures_come_first_with_their_error_text() {
        let mut broken = item("thing", Disposition::Failed, 10);
        broken.error = Some("permission denied".into());
        let text = report(vec![item("ok", Disposition::Purged, 1), broken], false);
        let failed_at = text.find("Failed · 1 item(s)").expect(&text);
        assert!(failed_at < text.find("Deleted").unwrap(), "{text}");
        assert!(text.contains("permission denied"), "{text}");
        assert!(text.contains("1 failed"), "{text}");
    }

    #[test]
    fn a_command_item_shows_what_ran() {
        let mut model = item("llama3.2:latest", Disposition::CommandRun, 2000);
        model.command = Some("ollama rm llama3.2:latest".into());
        let text = report(vec![model], false);
        assert!(text.contains("Cleaned by a command"), "{text}");
        assert!(text.contains("ran `ollama rm llama3.2:latest`"), "{text}");
    }

    #[test]
    fn a_dry_run_says_nothing_was_touched() {
        let text = report(vec![item("npm cache", Disposition::DryRun, 300)], true);
        assert!(text.contains("Would be reclaimed"), "{text}");
        assert!(
            text.contains("Dry run: 1 items, 300 B would be freed."),
            "{text}"
        );
    }

    #[test]
    fn several_paths_are_counted_not_listed() {
        let mut many = item("caches", Disposition::Purged, 5);
        many.paths = vec!["/home/t/a".into(), "/home/t/b".into(), "/home/t/c".into()];
        assert_eq!(
            where_it_was(&many, &Paths::with_home("/home/t")),
            "~/a (+2 more)"
        );
    }

    #[test]
    fn the_live_line_shows_the_bar_totals_and_current_item() {
        let mut progress = CleanProgress::new(false);
        progress.apply(CleanEvent::Started {
            total: 2,
            bytes: 10,
        });
        progress.apply(CleanEvent::ItemStarted {
            id: CandidateId("a".into()),
            label: "npm cache".into(),
        });
        progress.apply(CleanEvent::ItemFinished(Box::new(item(
            "npm cache",
            Disposition::Purged,
            4,
        ))));
        progress.apply(CleanEvent::ItemStarted {
            id: CandidateId("b".into()),
            label: "pip cache".into(),
        });
        let line = live_line(&progress, &Style::plain());
        assert!(line.contains("[##########----------]"), "{line}");
        assert!(line.contains("1 of 2 · 4 B freed"), "{line}");
        assert!(line.ends_with("pip cache"), "{line}");
    }
}
