//! Plain-text rendering for the non-interactive commands.
//!
//! The goal is that `reclaim scan` on its own answers the question the user
//! actually has — "what can I safely delete?" — without needing the TUI. So each
//! row carries size, staleness, tier and how it comes back, and the warnings are
//! printed underneath rather than truncated away.

use std::fmt::Write as _;

use reclaim_core::format::{bytes, ellipsize, relative_time};
use reclaim_core::journal::{Disposition, RunRecord};
use reclaim_core::model::{humanize_age, Candidate, Severity, Tier};
use reclaim_core::pipeline::ScanResult;
use reclaim_core::Paths;

/// ANSI styling, disabled when not writing to a terminal or when `--no-color`
/// or `NO_COLOR` is set.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn new(force_off: bool) -> Self {
        let enabled = !force_off
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::IsTerminal::is_terminal(&std::io::stdout());
        Self { enabled }
    }

    /// Styling disabled unconditionally. Used by tests so assertions match on
    /// text rather than on escape sequences.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    pub fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }

    pub fn red(&self, text: &str) -> String {
        self.wrap("31", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }

    /// Colour by risk: green is regenerable, yellow costs something, red may be
    /// irreplaceable.
    pub fn tier(&self, tier: Tier) -> String {
        match tier {
            Tier::Safe => self.green("safe"),
            Tier::Review => self.yellow("review"),
            Tier::Caution => self.red("caution"),
        }
    }

    pub fn severity(&self, severity: Severity, text: &str) -> String {
        match severity {
            Severity::Info => self.dim(text),
            Severity::Caution => self.yellow(text),
            Severity::Danger => self.red(text),
        }
    }
}

/// The full scan report.
pub fn scan_report(result: &ScanResult, paths: &Paths, style: &Style) -> String {
    let mut out = String::new();

    if result.candidates.is_empty() {
        let _ = writeln!(out, "Nothing to reclaim above the current size threshold.");
        append_hidden_note(&mut out, result, style);
        return out;
    }

    // The candidate list arrives ranked by score, which interleaves ecosystems.
    // Regroup for display — biggest ecosystem first, best candidate first within
    // it — so each heading is printed exactly once.
    for (group, group_size) in result.by_group() {
        let _ = writeln!(
            out,
            "\n{}  {}",
            style.bold(group.title()),
            style.dim(&format!("({})", bytes(group_size.on_disk)))
        );

        for candidate in result.candidates.iter().filter(|c| c.group == group) {
            let _ = writeln!(out, "{}", candidate_row(candidate, paths, style));

            for warning in &candidate.warnings {
                let _ = writeln!(
                    out,
                    "      {} {}",
                    style.severity(warning.severity, "!"),
                    style.severity(warning.severity, &warning.message)
                );
            }
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!(
            "Total reclaimable: {}",
            bytes(result.total_reclaimable())
        ))
    );
    append_hidden_note(&mut out, result, style);
    append_scan_footer(&mut out, result, style);

    out
}

/// One candidate as a single aligned line.
fn candidate_row(candidate: &Candidate, paths: &Paths, style: &Style) -> String {
    let size = format!("{:>9}", bytes(candidate.reclaimable()));
    let age = candidate
        .last_used_days()
        .map(humanize_age)
        .unwrap_or_else(|| "unknown".to_string());
    let path = candidate
        .primary_path()
        .map(|p| paths.contract(p))
        .unwrap_or_else(|| candidate.provider.clone());

    let extra = if candidate.paths.len() > 1 {
        format!(" (+{} more)", candidate.paths.len() - 1)
    } else {
        String::new()
    };

    let shared = candidate
        .size
        .filter(|s| s.shared > 0)
        .map(|s| format!("  {}", style.dim(&format!("[{} shared]", bytes(s.shared)))))
        .unwrap_or_default();

    format!(
        "  {}  {:<9}  {:<14}  {}{}\n      {}  {}{}",
        style.bold(&size),
        style.tier(candidate.tier),
        age,
        candidate.label,
        shared,
        style.dim(&ellipsize(&format!("{path}{extra}"), 60)),
        style.dim("→ "),
        style.dim(&candidate.regen.summary()),
    )
}

fn append_hidden_note(out: &mut String, result: &ScanResult, style: &Style) {
    let (count, hidden_bytes) = result.hidden();
    if count > 0 {
        let _ = writeln!(
            out,
            "{}",
            style.dim(&format!(
                "{count} more item(s) totalling {} hidden by the current filters. \
                 Use --all to include them.",
                bytes(hidden_bytes)
            ))
        );
    }
}

fn append_scan_footer(out: &mut String, result: &ScanResult, style: &Style) {
    let mut notes = vec![format!(
        "scanned {} project(s) in {:.1}s",
        result.projects_scanned,
        result.elapsed_ms as f64 / 1000.0
    )];

    // Never let a partial walk masquerade as an exact total.
    if !result.unreadable.is_empty() {
        notes.push(format!(
            "{} director(ies) could not be read, so totals are a lower bound",
            result.unreadable.len()
        ));
    }
    if result
        .candidates
        .iter()
        .any(|c| c.size.is_some_and(|s| s.partial))
    {
        notes.push("some sizes are incomplete due to permission errors".to_string());
    }

    let _ = writeln!(out, "{}", style.dim(&notes.join(" · ")));
}

/// Compact one-line-per-item summary, used before a `clean` confirmation.
pub fn clean_preview(candidates: &[Candidate], paths: &Paths, style: &Style) -> String {
    let mut out = String::new();
    for candidate in candidates {
        let _ = writeln!(
            out,
            "  {:>9}  {:<9}  {}  {}",
            bytes(candidate.reclaimable()),
            style.tier(candidate.tier),
            candidate.label,
            style.dim(&ellipsize(
                &candidate
                    .primary_path()
                    .map(|p| paths.contract(p))
                    .unwrap_or_default(),
                44
            ))
        );
    }
    out
}

/// Result of a completed run.
pub fn run_report(record: &RunRecord, style: &Style) -> String {
    let mut out = String::new();

    for item in &record.items {
        let (mark, colour): (&str, fn(&Style, &str) -> String) = match item.disposition {
            Disposition::Purged => ("removed", |s, t| s.green(t)),
            Disposition::Trashed => ("trashed", |s, t| s.green(t)),
            Disposition::CommandRun => ("cleaned", |s, t| s.green(t)),
            Disposition::DryRun => ("would remove", |s, t| s.cyan(t)),
            Disposition::Skipped => ("already gone", |s, t| s.dim(t)),
            Disposition::Failed => ("FAILED", |s, t| s.red(t)),
        };

        let _ = writeln!(
            out,
            "  {:<14} {:>9}  {}",
            colour(style, mark),
            bytes(if item.disposition == Disposition::DryRun {
                item.expected_bytes
            } else {
                item.freed_bytes
            }),
            item.label
        );

        if let Some(error) = &item.error {
            let _ = writeln!(out, "                 {}", style.red(error));
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "{}", style.bold(&record.summary()));

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

/// `reclaim history` table.
pub fn history_report(records: &[RunRecord], style: &Style) -> String {
    if records.is_empty() {
        return "No runs recorded yet.\n".to_string();
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!(
            "{:<22} {:<10} {:<10} {}",
            "WHEN", "TRIGGER", "FREED", "DETAIL"
        ))
    );

    for record in records {
        let when = relative_time(record.started_at);
        let trigger = format!("{:?}", record.trigger).to_lowercase();
        let _ = writeln!(
            out,
            "{:<22} {:<10} {:<10} {}",
            when,
            trigger,
            bytes(record.bytes_freed()),
            record.summary()
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reclaim_core::journal::{ItemOutcome, Trigger};
    use reclaim_core::model::{CandidateBuilder, CandidateId, Group, Signals, Size, Warning};
    use std::time::SystemTime;

    fn candidate(tier: Tier, on_disk: u64, days: u32, warnings: Vec<Warning>) -> Candidate {
        let mut builder = CandidateBuilder::new("test.thing", Group::Node, "npm cache")
            .path("/home/tester/.npm")
            .tier(tier);
        for warning in warnings {
            builder = builder.warn(warning);
        }
        builder.build().with_measurement(
            Size {
                on_disk,
                logical: on_disk,
                ..Size::default()
            },
            Signals {
                artifact_mtime: SystemTime::UNIX_EPOCH,
                artifact_atime: None,
                source_mtime: None,
                vcs_activity: None,
                last_used_days: days,
                active_now: false,
            },
        )
    }

    fn result(candidates: Vec<Candidate>) -> ScanResult {
        ScanResult {
            all: candidates.clone(),
            candidates,
            projects_scanned: 3,
            elapsed_ms: 1500,
            unreadable: Vec::new(),
        }
    }

    #[test]
    fn a_scan_row_carries_size_tier_age_and_regeneration_cost() {
        let paths = Paths::with_home("/home/tester");
        let text = scan_report(
            &result(vec![candidate(
                Tier::Safe,
                2 * 1024 * 1024 * 1024,
                120,
                vec![],
            )]),
            &paths,
            &Style::plain(),
        );

        assert!(text.contains("2.00 GB"), "{text}");
        assert!(text.contains("safe"), "{text}");
        assert!(text.contains("4 months ago"), "{text}");
        assert!(
            text.contains("~/.npm"),
            "paths must be shown relative to home: {text}"
        );
        assert!(
            text.contains("auto on"),
            "regeneration cost must be shown: {text}"
        );
    }

    #[test]
    fn warnings_are_printed_not_truncated_away() {
        // The whole point of the tool is the evidence, so it must never be elided.
        let paths = Paths::with_home("/home/tester");
        let text = scan_report(
            &result(vec![candidate(
                Tier::Caution,
                1024,
                400,
                vec![Warning::danger("Contains dSYMs for shipped builds.")],
            )]),
            &paths,
            &Style::plain(),
        );
        assert!(
            text.contains("Contains dSYMs for shipped builds."),
            "{text}"
        );
    }

    #[test]
    fn each_ecosystem_heading_is_printed_exactly_once() {
        // The ranked list interleaves ecosystems, so a naive "heading changed?"
        // check prints `Rust` twice with a Node row between them.
        let paths = Paths::with_home("/home/tester");
        let mut rust_big = candidate(Tier::Safe, 20 * 1024 * 1024 * 1024, 300, vec![]);
        rust_big.group = Group::Rust;
        rust_big.id = CandidateId("rust:1".into());

        let mut node = candidate(Tier::Safe, 10 * 1024 * 1024 * 1024, 300, vec![]);
        node.group = Group::Node;
        node.id = CandidateId("node:1".into());

        let mut rust_small = candidate(Tier::Safe, 1024 * 1024 * 1024, 300, vec![]);
        rust_small.group = Group::Rust;
        rust_small.id = CandidateId("rust:2".into());

        let text = scan_report(
            &result(vec![rust_big, node, rust_small]),
            &paths,
            &Style::plain(),
        );

        assert_eq!(
            text.matches("Rust").count(),
            1,
            "duplicate Rust heading:\n{text}"
        );
        assert_eq!(
            text.matches("Node.js").count(),
            1,
            "duplicate Node heading:\n{text}"
        );
        // Biggest ecosystem first.
        assert!(
            text.find("Rust").unwrap() < text.find("Node.js").unwrap(),
            "groups should be ordered by size:\n{text}"
        );
    }

    #[test]
    fn an_empty_result_says_so_plainly() {
        let paths = Paths::with_home("/home/tester");
        let text = scan_report(&result(vec![]), &paths, &Style::plain());
        assert!(text.contains("Nothing to reclaim"), "{text}");
    }

    #[test]
    fn hidden_items_are_accounted_for_rather_than_dropped() {
        let paths = Paths::with_home("/home/tester");
        let shown = candidate(Tier::Safe, 5 * 1024 * 1024 * 1024, 100, vec![]);
        let mut hidden = candidate(Tier::Safe, 1024, 100, vec![]);
        hidden.id = CandidateId("other:1".into());

        let result = ScanResult {
            candidates: vec![shown.clone()],
            all: vec![shown, hidden],
            projects_scanned: 1,
            elapsed_ms: 100,
            unreadable: Vec::new(),
        };

        let text = scan_report(&result, &paths, &Style::plain());
        assert!(text.contains("1 more item"), "{text}");
        assert!(
            text.contains("--all"),
            "the user must be told how to see them: {text}"
        );
    }

    #[test]
    fn a_partial_walk_is_reported_as_a_lower_bound() {
        let paths = Paths::with_home("/home/tester");
        let mut r = result(vec![candidate(Tier::Safe, 1024, 100, vec![])]);
        r.unreadable = vec!["/home/tester/.locked".into()];

        let text = scan_report(&r, &paths, &Style::plain());
        assert!(
            text.contains("lower bound"),
            "totals must not look exact: {text}"
        );
    }

    #[test]
    fn shared_bytes_are_surfaced_on_the_row() {
        let paths = Paths::with_home("/home/tester");
        let mut c = candidate(Tier::Review, 1024 * 1024, 100, vec![]);
        c.size = Some(Size {
            on_disk: 1024 * 1024,
            shared: 8 * 1024 * 1024,
            ..Size::default()
        });

        let text = scan_report(&result(vec![c]), &paths, &Style::plain());
        assert!(
            text.contains("shared"),
            "hardlinked bytes must be visible: {text}"
        );
    }

    #[test]
    fn a_run_report_separates_trashed_from_freed() {
        let mut record = RunRecord::new(Trigger::Cli, false);
        record.items = vec![
            ItemOutcome {
                id: CandidateId("a".into()),
                provider: "node.npm-cache".into(),
                group: Group::Node,
                label: "npm cache".into(),
                tier: Tier::Safe,
                paths: vec!["/home/t/.npm".into()],
                expected_bytes: 1000,
                freed_bytes: 1000,
                disposition: Disposition::Purged,
                error: None,
            },
            ItemOutcome {
                id: CandidateId("b".into()),
                provider: "apple.archives".into(),
                group: Group::Apple,
                label: "Xcode Archives".into(),
                tier: Tier::Caution,
                paths: vec!["/home/t/Archives".into()],
                expected_bytes: 5000,
                freed_bytes: 5000,
                disposition: Disposition::Trashed,
                error: None,
            },
        ];

        let text = run_report(&record, &Style::plain());
        assert!(text.contains("removed"), "{text}");
        assert!(text.contains("trashed"), "{text}");
        assert!(
            text.contains("still occupies disk"),
            "trashed bytes must not be presented as freed: {text}"
        );
    }

    #[test]
    fn failures_show_their_error_text() {
        let mut record = RunRecord::new(Trigger::Cli, false);
        record.items = vec![ItemOutcome {
            id: CandidateId("a".into()),
            provider: "x.y".into(),
            group: Group::Node,
            label: "thing".into(),
            tier: Tier::Safe,
            paths: vec!["/x".into()],
            expected_bytes: 10,
            freed_bytes: 0,
            disposition: Disposition::Failed,
            error: Some("permission denied".into()),
        }];

        let text = run_report(&record, &Style::plain());
        assert!(text.contains("FAILED"), "{text}");
        assert!(text.contains("permission denied"), "{text}");
    }

    #[test]
    fn styling_is_inert_when_disabled() {
        let plain = Style::plain();
        assert_eq!(plain.red("x"), "x");
        assert_eq!(plain.tier(Tier::Caution), "caution");
    }

    #[test]
    fn history_is_readable_when_empty() {
        assert!(history_report(&[], &Style::plain()).contains("No runs recorded"));
    }
}
