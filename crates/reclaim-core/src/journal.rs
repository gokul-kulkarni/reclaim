//! Run history.
//!
//! Every run that could delete something writes a record here, including dry runs
//! and background runs. Three reasons this exists: `reclaim history` for the user,
//! auditability for the Phase 2 scheduled runs that happen while nobody is
//! watching, and an honest record of what a run *actually* did when it partially
//! failed.
//!
//! Records are one JSON file per run rather than an appended log, so a crash
//! mid-write can only corrupt the run that was in flight.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{CandidateId, Group, Tier};

/// How a run was started, which is what makes background activity auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// Interactive TUI.
    Tui,
    /// Non-interactive `reclaim clean`.
    Cli,
    /// The local web UI.
    Web,
    /// A launchd / systemd timer.
    Scheduled,
}

/// What happened to one candidate in a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub id: CandidateId,
    pub provider: String,
    pub group: Group,
    pub label: String,
    pub tier: Tier,
    pub paths: Vec<PathBuf>,
    /// Bytes we expected to free, from the scan.
    pub expected_bytes: u64,
    /// Bytes actually freed. Differs from `expected_bytes` on partial failure.
    pub freed_bytes: u64,
    pub disposition: Disposition,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    /// Permanently removed.
    Purged,
    /// Moved to the OS Trash; space is not freed until the Trash is emptied.
    Trashed,
    /// Reclaimed by an external command.
    CommandRun,
    /// Reported only; nothing was touched.
    DryRun,
    /// Refused by the safety guard or failed on the filesystem.
    Failed,
    /// Already gone by the time we got there.
    Skipped,
}

impl Disposition {
    /// Whether this outcome actually returned space to the filesystem now.
    ///
    /// Trashed items deliberately count as zero: a tool that reports "freed 40 GB"
    /// while the disk is unchanged because it all sits in the Trash is lying.
    pub fn frees_space_immediately(self) -> bool {
        matches!(self, Disposition::Purged | Disposition::CommandRun)
    }
}

/// One complete run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub trigger: Trigger,
    pub dry_run: bool,
    /// Every candidate the scan offered, whether or not it was chosen.
    pub candidates_found: usize,
    pub bytes_found: u64,
    pub items: Vec<ItemOutcome>,
    /// Version of `reclaim` that produced this record.
    pub version: String,
}

impl RunRecord {
    pub fn new(trigger: Trigger, dry_run: bool) -> Self {
        let now = SystemTime::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            started_at: now,
            finished_at: now,
            trigger,
            dry_run,
            candidates_found: 0,
            bytes_found: 0,
            items: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Bytes genuinely returned to the filesystem by this run.
    pub fn bytes_freed(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.disposition.frees_space_immediately())
            .map(|i| i.freed_bytes)
            .sum()
    }

    /// Bytes moved to the Trash: recoverable, and not yet reflected in free space.
    pub fn bytes_trashed(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.disposition == Disposition::Trashed)
            .map(|i| i.freed_bytes)
            .sum()
    }

    pub fn failures(&self) -> impl Iterator<Item = &ItemOutcome> {
        self.items
            .iter()
            .filter(|i| i.disposition == Disposition::Failed)
    }

    pub fn succeeded(&self) -> bool {
        self.failures().next().is_none()
    }

    /// One-line summary for notifications and the end of a CLI run.
    pub fn summary(&self) -> String {
        use crate::format::bytes;
        if self.dry_run {
            return format!(
                "dry run: {} items, {} would be freed",
                self.items.len(),
                bytes(self.items.iter().map(|i| i.expected_bytes).sum())
            );
        }
        let freed = self.bytes_freed();
        let trashed = self.bytes_trashed();
        let mut parts = vec![format!("freed {}", bytes(freed))];
        if trashed > 0 {
            parts.push(format!("{} moved to Trash", bytes(trashed)));
        }
        let failures = self.failures().count();
        if failures > 0 {
            parts.push(format!("{failures} failed"));
        }
        parts.join(", ")
    }
}

/// Append-only store of run records on disk.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist a record. Filenames sort chronologically so `read_recent` can rely
    /// on name order rather than stat-ing every file.
    pub fn write(&self, record: &RunRecord) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir).map_err(|e| Error::io(&self.dir, e))?;

        let stamp = record
            .started_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let short_id = record.id.split('-').next().unwrap_or("run");
        let path = self.dir.join(format!("{stamp:012}-{short_id}.json"));

        let json = serde_json::to_string_pretty(record)
            .map_err(|e| Error::Other(format!("serialising run record: {e}")))?;
        std::fs::write(&path, json).map_err(|e| Error::io(&path, e))?;
        Ok(path)
    }

    /// Most recent runs, newest first. Unreadable or malformed files are skipped:
    /// a corrupt old record must never stop the tool from running today.
    pub fn read_recent(&self, limit: usize) -> Vec<RunRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };

        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        names.sort();
        names.reverse();

        names
            .into_iter()
            .filter_map(|p| std::fs::read_to_string(&p).ok())
            .filter_map(|text| serde_json::from_str::<RunRecord>(&text).ok())
            .take(limit)
            .collect()
    }

    pub fn last(&self) -> Option<RunRecord> {
        self.read_recent(1).into_iter().next()
    }

    /// Drop records older than `keep`, so the journal cannot grow without bound.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(0);
        };
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        if names.len() <= keep {
            return Ok(0);
        }
        names.sort();
        let doomed = names.len() - keep;
        let mut removed = 0;
        for path in names.into_iter().take(doomed) {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn item(disposition: Disposition, bytes: u64) -> ItemOutcome {
        ItemOutcome {
            id: CandidateId("test:1".into()),
            provider: "node.npm-cache".into(),
            group: Group::Node,
            label: "npm cache".into(),
            tier: Tier::Safe,
            paths: vec![PathBuf::from("/home/t/.npm")],
            expected_bytes: bytes,
            freed_bytes: bytes,
            disposition,
            error: None,
        }
    }

    #[test]
    fn trashed_bytes_are_not_counted_as_freed() {
        // A tool that says "freed 40 GB" while the disk is unchanged is lying.
        let mut record = RunRecord::new(Trigger::Cli, false);
        record.items = vec![
            item(Disposition::Purged, 1000),
            item(Disposition::Trashed, 5000),
        ];
        assert_eq!(record.bytes_freed(), 1000);
        assert_eq!(record.bytes_trashed(), 5000);
    }

    #[test]
    fn dry_runs_report_what_would_have_happened() {
        let mut record = RunRecord::new(Trigger::Cli, true);
        record.items = vec![item(Disposition::DryRun, 2048)];
        assert_eq!(record.bytes_freed(), 0);
        assert!(
            record.summary().starts_with("dry run"),
            "{}",
            record.summary()
        );
    }

    #[test]
    fn failures_are_surfaced_in_the_summary() {
        let mut record = RunRecord::new(Trigger::Cli, false);
        let mut failed = item(Disposition::Failed, 0);
        failed.error = Some("permission denied".into());
        record.items = vec![item(Disposition::Purged, 100), failed];
        assert!(!record.succeeded());
        assert_eq!(record.failures().count(), 1);
        assert!(
            record.summary().contains("1 failed"),
            "{}",
            record.summary()
        );
    }

    #[test]
    fn records_roundtrip_through_the_journal() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::new(tmp.path());
        let mut record = RunRecord::new(Trigger::Scheduled, false);
        record.items = vec![item(Disposition::Purged, 4096)];
        record.candidates_found = 7;

        journal.write(&record).unwrap();
        let read = journal.last().unwrap();
        assert_eq!(read.id, record.id);
        assert_eq!(read.trigger, Trigger::Scheduled);
        assert_eq!(read.candidates_found, 7);
        assert_eq!(read.bytes_freed(), 4096);
    }

    #[test]
    fn recent_runs_come_back_newest_first() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::new(tmp.path());
        for i in 0..3u64 {
            let mut r = RunRecord::new(Trigger::Cli, false);
            r.started_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i);
            r.candidates_found = i as usize;
            journal.write(&r).unwrap();
        }
        let recent = journal.read_recent(10);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].candidates_found, 2, "newest first");
    }

    #[test]
    fn a_corrupt_record_does_not_break_reading_the_others() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::new(tmp.path());
        journal.write(&RunRecord::new(Trigger::Cli, false)).unwrap();
        std::fs::write(tmp.path().join("999999999999-bad.json"), "{ not json").unwrap();
        assert_eq!(journal.read_recent(10).len(), 1);
    }

    #[test]
    fn reading_a_missing_journal_directory_is_empty_not_an_error() {
        let journal = Journal::new("/nonexistent/reclaim/history");
        assert!(journal.read_recent(5).is_empty());
        assert!(journal.last().is_none());
    }

    #[test]
    fn prune_keeps_the_newest_records() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::new(tmp.path());
        for i in 0..5u64 {
            let mut r = RunRecord::new(Trigger::Cli, false);
            r.started_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i);
            r.candidates_found = i as usize;
            journal.write(&r).unwrap();
        }
        assert_eq!(journal.prune(2).unwrap(), 3);
        let left = journal.read_recent(10);
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].candidates_found, 4);
    }
}
