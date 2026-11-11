//! Running totals for a clean in flight, built from [`CleanEvent`]s.
//!
//! The terminal UI, `reclaim clean` and the web server all show the same
//! numbers while a run executes; folding the events here keeps them from
//! disagreeing about what "freed so far" means.

use crate::exec::CleanEvent;
use crate::journal::{Disposition, ItemOutcome, RunRecord};

/// What a run has done so far. Built by [`CleanProgress::apply`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CleanProgress {
    pub dry_run: bool,
    /// Items in the run.
    pub total: usize,
    /// Bytes the scan expected the run to reclaim.
    pub bytes_total: u64,
    /// Items finished, whatever their outcome.
    pub done: usize,
    /// Bytes returned to the filesystem now (removed or cleaned by a command).
    pub freed: u64,
    /// Bytes moved to the Trash: still on disk until it is emptied.
    pub trashed: u64,
    /// Bytes a dry run would have reclaimed.
    pub would_free: u64,
    pub failed: usize,
    /// Labels of items started and not yet finished, oldest first.
    pub in_flight: Vec<String>,
    /// Finished items, in the order they finished.
    pub finished: Vec<ItemOutcome>,
    /// Set once the run is over.
    pub record: Option<RunRecord>,
}

impl CleanProgress {
    pub fn new(dry_run: bool) -> Self {
        Self {
            dry_run,
            ..Self::default()
        }
    }

    /// Fold one event into the totals.
    pub fn apply(&mut self, event: CleanEvent) {
        match event {
            CleanEvent::Started { total, bytes } => {
                self.total = total;
                self.bytes_total = bytes;
            }
            CleanEvent::ItemStarted { label, .. } => self.in_flight.push(label),
            CleanEvent::ItemFinished(outcome) => self.finish_item(*outcome),
            CleanEvent::Finished(record) => {
                self.in_flight.clear();
                self.record = Some(*record);
            }
        }
    }

    fn finish_item(&mut self, outcome: ItemOutcome) {
        // Two items can share a label; drop one occurrence, not all of them.
        if let Some(at) = self.in_flight.iter().position(|l| *l == outcome.label) {
            self.in_flight.remove(at);
        }
        match outcome.disposition {
            Disposition::Purged | Disposition::CommandRun => self.freed += outcome.freed_bytes,
            Disposition::Trashed => self.trashed += outcome.freed_bytes,
            Disposition::DryRun => self.would_free += outcome.expected_bytes,
            Disposition::Failed => self.failed += 1,
            Disposition::Skipped => {}
        }
        self.done += 1;
        self.finished.push(outcome);
    }

    pub fn is_finished(&self) -> bool {
        self.record.is_some()
    }

    /// Fraction of items finished, 0.0 to 1.0.
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            return if self.is_finished() { 1.0 } else { 0.0 };
        }
        (self.done as f64 / self.total as f64).min(1.0)
    }

    /// The item to name in a one-line status: the oldest one still running.
    pub fn current(&self) -> Option<&str> {
        self.in_flight.first().map(String::as_str)
    }

    /// `3 of 12 · 1.2 GB freed · 400 MB to Trash · 1 failed`
    pub fn status_line(&self) -> String {
        use crate::format::bytes;
        let mut parts = vec![format!("{} of {}", self.done, self.total)];
        if self.dry_run {
            parts.push(format!("{} would be freed", bytes(self.would_free)));
        } else {
            parts.push(format!("{} freed", bytes(self.freed)));
            if self.trashed > 0 {
                parts.push(format!("{} to Trash", bytes(self.trashed)));
            }
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        parts.join(" · ")
    }
}

/// How a finished item is presented: a heading for its section and where the
/// data went. Shared so the CLI, TUI and web UI use the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutcomeSection {
    Failed,
    Removed,
    Command,
    Trashed,
    DryRun,
    AlreadyGone,
}

impl OutcomeSection {
    /// Failures first: they are what the user has to act on.
    pub const ORDER: [OutcomeSection; 6] = [
        OutcomeSection::Failed,
        OutcomeSection::Removed,
        OutcomeSection::Command,
        OutcomeSection::Trashed,
        OutcomeSection::DryRun,
        OutcomeSection::AlreadyGone,
    ];

    pub fn of(disposition: Disposition) -> Self {
        match disposition {
            Disposition::Failed => Self::Failed,
            Disposition::Purged => Self::Removed,
            Disposition::CommandRun => Self::Command,
            Disposition::Trashed => Self::Trashed,
            Disposition::DryRun => Self::DryRun,
            Disposition::Skipped => Self::AlreadyGone,
        }
    }

    pub fn heading(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Removed => "Deleted",
            Self::Command => "Cleaned by a command",
            Self::Trashed => "Moved to the Trash",
            Self::DryRun => "Would be reclaimed",
            Self::AlreadyGone => "Already gone",
        }
    }

    /// One sentence on what this outcome means for the user's disk.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Failed => "Nothing was removed for these items.",
            Self::Removed => "Permanently deleted. The space is free now.",
            Self::Command => "The owning tool reclaimed this space itself.",
            Self::Trashed => "Recoverable from the Trash. The space returns when you empty it.",
            Self::DryRun => "Dry run: nothing was touched.",
            Self::AlreadyGone => "These no longer existed when the run reached them.",
        }
    }
}

/// Items of `record` grouped into sections, in [`OutcomeSection::ORDER`],
/// largest first within each, with empty sections left out.
pub fn sections(items: &[ItemOutcome]) -> Vec<(OutcomeSection, Vec<&ItemOutcome>)> {
    OutcomeSection::ORDER
        .iter()
        .filter_map(|&section| {
            let mut members: Vec<&ItemOutcome> = items
                .iter()
                .filter(|i| OutcomeSection::of(i.disposition) == section)
                .collect();
            members.sort_by_key(|i| std::cmp::Reverse(shown_bytes(i)));
            (!members.is_empty()).then_some((section, members))
        })
        .collect()
}

/// The size to show for an item: what it freed, or what it was expected to
/// free when nothing was freed (a dry run, a failure, or something gone).
pub fn shown_bytes(item: &ItemOutcome) -> u64 {
    match item.disposition {
        Disposition::Purged | Disposition::CommandRun | Disposition::Trashed => item.freed_bytes,
        _ => item.expected_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Trigger;
    use crate::model::{CandidateId, Group, Tier};
    use std::path::PathBuf;

    fn item(label: &str, disposition: Disposition, bytes: u64) -> ItemOutcome {
        let freed = match disposition {
            Disposition::Purged | Disposition::CommandRun | Disposition::Trashed => bytes,
            _ => 0,
        };
        ItemOutcome {
            id: CandidateId(format!("t:{label}")),
            provider: "node.npm-cache".into(),
            group: Group::Node,
            label: label.into(),
            tier: Tier::Safe,
            paths: vec![PathBuf::from("/home/t/.npm")],
            expected_bytes: bytes,
            freed_bytes: freed,
            disposition,
            error: None,
            command: None,
        }
    }

    fn started(label: &str) -> CleanEvent {
        CleanEvent::ItemStarted {
            id: CandidateId(format!("t:{label}")),
            label: label.into(),
        }
    }

    fn finished(item: ItemOutcome) -> CleanEvent {
        CleanEvent::ItemFinished(Box::new(item))
    }

    #[test]
    fn totals_separate_freed_trashed_and_failed() {
        let mut p = CleanProgress::new(false);
        p.apply(CleanEvent::Started {
            total: 3,
            bytes: 600,
        });
        p.apply(started("a"));
        p.apply(started("b"));
        assert_eq!(p.current(), Some("a"));
        p.apply(finished(item("a", Disposition::Purged, 100)));
        assert_eq!(p.current(), Some("b"));
        p.apply(finished(item("b", Disposition::Trashed, 200)));
        p.apply(started("c"));
        p.apply(finished(item("c", Disposition::Failed, 300)));

        assert_eq!((p.done, p.freed, p.trashed, p.failed), (3, 100, 200, 1));
        assert!((p.ratio() - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            p.status_line(),
            "3 of 3 · 100 B freed · 200 B to Trash · 1 failed"
        );
        assert!(!p.is_finished());
        p.apply(CleanEvent::Finished(Box::new(RunRecord::new(
            Trigger::Cli,
            false,
        ))));
        assert!(p.is_finished());
        assert!(p.in_flight.is_empty());
    }

    #[test]
    fn a_dry_run_counts_what_would_be_freed() {
        let mut p = CleanProgress::new(true);
        p.apply(CleanEvent::Started {
            total: 2,
            bytes: 50,
        });
        p.apply(finished(item("a", Disposition::DryRun, 50)));
        assert_eq!(p.status_line(), "1 of 2 · 50 B would be freed");
        assert!((p.ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn duplicate_labels_leave_one_in_flight() {
        let mut p = CleanProgress::new(false);
        p.apply(started("cache"));
        p.apply(started("cache"));
        p.apply(finished(item("cache", Disposition::Purged, 1)));
        assert_eq!(p.in_flight, ["cache"]);
    }

    #[test]
    fn sections_put_failures_first_and_sort_by_size() {
        let items = vec![
            item("small", Disposition::Purged, 10),
            item("broken", Disposition::Failed, 5),
            item("big", Disposition::Purged, 90),
            item("binned", Disposition::Trashed, 40),
        ];
        let grouped = sections(&items);
        let order: Vec<OutcomeSection> = grouped.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            order,
            [
                OutcomeSection::Failed,
                OutcomeSection::Removed,
                OutcomeSection::Trashed
            ]
        );
        let removed: Vec<&str> = grouped[1].1.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(removed, ["big", "small"]);
        assert_eq!(
            shown_bytes(grouped[0].1[0]),
            5,
            "a failure shows what it held"
        );
    }
}
