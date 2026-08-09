//! Aggregate reporting over run history.
//!
//! `reclaim history` shows one line per run; that answers "what happened
//! last time" but not "what has reclaim actually done for me". This module
//! turns a list of `RunRecord`s into the statistics that answer the second
//! question — lifetime totals, a breakdown by ecosystem and by trigger, a
//! timeline, the biggest single reclaims ever, and recurring failures worth
//! investigating.
//!
//! Both the CLI's HTML export and the web UI's History tab render this same
//! structure, so the two surfaces can never disagree about a number.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::journal::{Disposition, RunRecord, Trigger};
use crate::model::{Group, Tier};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryReport {
    pub generated_at: SystemTime,
    pub runs: usize,
    pub real_runs: usize,
    pub dry_runs: usize,
    pub lifetime_freed: u64,
    pub lifetime_trashed: u64,
    pub lifetime_candidates_found: u64,
    pub failed_items: usize,
    /// Largest ecosystem first.
    pub by_group: Vec<GroupStats>,
    /// Most-used trigger first.
    pub by_trigger: Vec<TriggerStats>,
    /// Chronological, real runs only: a dry run reclaims nothing to plot.
    pub timeline: Vec<TimelinePoint>,
    /// Biggest single reclaims ever, largest first.
    pub top_items: Vec<TopItem>,
    /// Most recent failures first.
    pub failures: Vec<FailureEntry>,
    /// Most recent runs first, matching `reclaim history`'s ordering.
    pub runs_detail: Vec<RunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStats {
    pub group: Group,
    pub title: String,
    pub freed: u64,
    pub trashed: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerStats {
    pub trigger: Trigger,
    pub label: String,
    pub runs: usize,
    pub freed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePoint {
    pub started_at: SystemTime,
    pub freed: u64,
    pub trashed: u64,
    pub cumulative_freed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopItem {
    pub label: String,
    pub group: Group,
    pub tier: Tier,
    pub bytes: u64,
    pub started_at: SystemTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub started_at: SystemTime,
    pub label: String,
    pub provider: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    pub started_at: SystemTime,
    pub trigger: Trigger,
    pub dry_run: bool,
    pub candidates_found: usize,
    pub freed: u64,
    pub trashed: u64,
    pub items: usize,
    pub failures: usize,
    pub succeeded: bool,
}

/// How many entries to keep in ranked lists. Unbounded lists would make an old,
/// well-used journal render a report thousands of rows long for no benefit —
/// nobody needs to see the 400th-biggest reclaim.
const TOP_ITEMS_LIMIT: usize = 15;
const FAILURES_LIMIT: usize = 25;

impl HistoryReport {
    /// Build a report from records in any order — `Journal::read_recent`
    /// returns newest-first, but this sorts internally, so callers never need
    /// to think about it.
    pub fn build(records: &[RunRecord]) -> Self {
        let mut chronological: Vec<&RunRecord> = records.iter().collect();
        chronological.sort_by_key(|r| r.started_at);

        let dry_runs = records.iter().filter(|r| r.dry_run).count();

        let mut lifetime_freed = 0u64;
        let mut lifetime_trashed = 0u64;
        let mut lifetime_candidates_found = 0u64;
        let mut failed_items = 0usize;

        let mut by_group: BTreeMap<Group, GroupStats> = BTreeMap::new();
        let mut by_trigger: BTreeMap<Trigger, TriggerStats> = BTreeMap::new();
        let mut top_items: Vec<TopItem> = Vec::new();
        let mut failures: Vec<FailureEntry> = Vec::new();
        let mut runs_detail: Vec<RunSummary> = Vec::new();
        let mut timeline: Vec<TimelinePoint> = Vec::new();
        let mut cumulative = 0u64;

        for record in &chronological {
            lifetime_candidates_found += record.candidates_found as u64;

            let freed = record.bytes_freed();
            let trashed = record.bytes_trashed();

            if !record.dry_run {
                lifetime_freed += freed;
                lifetime_trashed += trashed;
                cumulative += freed;
                timeline.push(TimelinePoint {
                    started_at: record.started_at,
                    freed,
                    trashed,
                    cumulative_freed: cumulative,
                });
            }

            let trigger_entry = by_trigger.entry(record.trigger).or_insert_with(|| TriggerStats {
                trigger: record.trigger,
                label: record.trigger.label().to_string(),
                runs: 0,
                freed: 0,
            });
            trigger_entry.runs += 1;
            trigger_entry.freed += freed;

            for item in &record.items {
                if item.disposition == Disposition::Failed {
                    failed_items += 1;
                    failures.push(FailureEntry {
                        started_at: record.started_at,
                        label: item.label.clone(),
                        provider: item.provider.clone(),
                        error: item
                            .error
                            .clone()
                            .unwrap_or_else(|| "unknown error".to_string()),
                    });
                    continue;
                }
                if item.freed_bytes == 0 {
                    continue;
                }

                let group_entry = by_group.entry(item.group).or_insert_with(|| GroupStats {
                    group: item.group,
                    title: item.group.title().to_string(),
                    freed: 0,
                    trashed: 0,
                    items: 0,
                });
                group_entry.items += 1;
                match item.disposition {
                    Disposition::Purged | Disposition::CommandRun => {
                        group_entry.freed += item.freed_bytes
                    }
                    Disposition::Trashed => group_entry.trashed += item.freed_bytes,
                    _ => {}
                }

                if !record.dry_run && item.disposition.frees_space_immediately() {
                    top_items.push(TopItem {
                        label: item.label.clone(),
                        group: item.group,
                        tier: item.tier,
                        bytes: item.freed_bytes,
                        started_at: record.started_at,
                    });
                }
            }

            runs_detail.push(RunSummary {
                id: record.id.clone(),
                started_at: record.started_at,
                trigger: record.trigger,
                dry_run: record.dry_run,
                candidates_found: record.candidates_found,
                freed,
                trashed,
                items: record.items.len(),
                failures: record.failures().count(),
                succeeded: record.succeeded(),
            });
        }

        top_items.sort_by_key(|i| std::cmp::Reverse(i.bytes));
        top_items.truncate(TOP_ITEMS_LIMIT);

        failures.sort_by_key(|f| std::cmp::Reverse(f.started_at));
        failures.truncate(FAILURES_LIMIT);

        runs_detail.sort_by_key(|r| std::cmp::Reverse(r.started_at));

        let mut by_group: Vec<GroupStats> = by_group.into_values().collect();
        by_group.sort_by_key(|g| std::cmp::Reverse(g.freed + g.trashed));

        let mut by_trigger: Vec<TriggerStats> = by_trigger.into_values().collect();
        by_trigger.sort_by_key(|t| std::cmp::Reverse(t.runs));

        HistoryReport {
            generated_at: SystemTime::now(),
            runs: records.len(),
            real_runs: records.len() - dry_runs,
            dry_runs,
            lifetime_freed,
            lifetime_trashed,
            lifetime_candidates_found,
            failed_items,
            by_group,
            by_trigger,
            timeline,
            top_items,
            failures,
            runs_detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::ItemOutcome;
    use crate::model::CandidateId;
    use std::path::PathBuf;
    use std::time::Duration;

    fn item(group: Group, tier: Tier, disposition: Disposition, bytes: u64) -> ItemOutcome {
        ItemOutcome {
            id: CandidateId("t:1".into()),
            provider: format!("{}.thing", group.as_str()),
            group,
            label: format!("{} cache", group.as_str()),
            tier,
            paths: vec![PathBuf::from("/home/t/.cache")],
            expected_bytes: bytes,
            freed_bytes: bytes,
            disposition,
            error: None,
        }
    }

    fn failed_item(label: &str, error: &str) -> ItemOutcome {
        let mut i = item(Group::Node, Tier::Safe, Disposition::Failed, 0);
        i.label = label.to_string();
        i.error = Some(error.to_string());
        i
    }

    fn run_at(secs: u64, trigger: Trigger, dry_run: bool, items: Vec<ItemOutcome>) -> RunRecord {
        let mut r = RunRecord::new(trigger, dry_run);
        r.started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        r.finished_at = r.started_at;
        r.candidates_found = items.len();
        r.items = items;
        r
    }

    #[test]
    fn lifetime_totals_exclude_dry_runs_and_trashed_items() {
        let records = vec![
            run_at(
                1,
                Trigger::Cli,
                false,
                vec![
                    item(Group::Node, Tier::Safe, Disposition::Purged, 1000),
                    item(Group::Jvm, Tier::Review, Disposition::Trashed, 5000),
                ],
            ),
            run_at(
                2,
                Trigger::Cli,
                true,
                vec![item(Group::Rust, Tier::Safe, Disposition::DryRun, 9_000_000)],
            ),
        ];

        let report = HistoryReport::build(&records);
        assert_eq!(report.runs, 2);
        assert_eq!(report.real_runs, 1);
        assert_eq!(report.dry_runs, 1);
        assert_eq!(report.lifetime_freed, 1000, "trashed and dry-run bytes must not count as freed");
        assert_eq!(report.lifetime_trashed, 5000);
    }

    #[test]
    fn by_group_ranks_the_largest_ecosystem_first() {
        let records = vec![run_at(
            1,
            Trigger::Cli,
            false,
            vec![
                item(Group::Node, Tier::Safe, Disposition::Purged, 1_000),
                item(Group::Jvm, Tier::Safe, Disposition::Purged, 50_000),
            ],
        )];

        let report = HistoryReport::build(&records);
        assert_eq!(report.by_group[0].group, Group::Jvm);
        assert_eq!(report.by_group[0].freed, 50_000);
        assert_eq!(report.by_group[1].group, Group::Node);
    }

    #[test]
    fn timeline_is_chronological_with_a_running_total() {
        let records = vec![
            run_at(
                200,
                Trigger::Cli,
                false,
                vec![item(Group::Node, Tier::Safe, Disposition::Purged, 100)],
            ),
            run_at(
                100,
                Trigger::Cli,
                false,
                vec![item(Group::Node, Tier::Safe, Disposition::Purged, 50)],
            ),
        ];

        // Passed in reverse (as Journal::read_recent would return them).
        let report = HistoryReport::build(&records);
        assert_eq!(report.timeline.len(), 2);
        assert_eq!(report.timeline[0].started_at, records[1].started_at, "oldest first");
        assert_eq!(report.timeline[0].cumulative_freed, 50);
        assert_eq!(report.timeline[1].cumulative_freed, 150);
    }

    #[test]
    fn failures_are_collected_across_runs_newest_first() {
        let records = vec![
            run_at(1, Trigger::Scheduled, false, vec![failed_item("a", "permission denied")]),
            run_at(2, Trigger::Scheduled, false, vec![failed_item("b", "not found")]),
        ];

        let report = HistoryReport::build(&records);
        assert_eq!(report.failed_items, 2);
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.failures[0].label, "b", "most recent failure first");
        assert_eq!(report.failures[0].error, "not found");
    }

    #[test]
    fn top_items_are_capped_and_sorted_descending() {
        let items: Vec<ItemOutcome> = (0..20)
            .map(|i| item(Group::Node, Tier::Safe, Disposition::Purged, i * 10))
            .collect();
        let records = vec![run_at(1, Trigger::Cli, false, items)];

        let report = HistoryReport::build(&records);
        assert_eq!(report.top_items.len(), TOP_ITEMS_LIMIT);
        assert_eq!(report.top_items[0].bytes, 190, "largest first");
        assert!(report.top_items.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }

    #[test]
    fn an_empty_journal_produces_a_zeroed_report_not_a_panic() {
        let report = HistoryReport::build(&[]);
        assert_eq!(report.runs, 0);
        assert_eq!(report.lifetime_freed, 0);
        assert!(report.by_group.is_empty());
        assert!(report.timeline.is_empty());
    }
}
