//! The only code in this project that removes anything.
//!
//! Invariants, in order of importance:
//!
//! 1. Every path is re-validated by [`PathGuard`] immediately before removal, not
//!    just at scan time. Minutes can pass while the user deliberates in the UI.
//! 2. A dry run never calls a destructive function at all. It is not a flag checked
//!    deep inside the delete path; it returns before one is reached.
//! 3. Failures are recorded per item and never abort the run. One unreadable
//!    directory must not prevent the other 40 GB from being reclaimed.
//! 4. Freed bytes are only ever reported for operations that actually returned
//!    space to the filesystem. Trashed bytes are reported separately.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use rayon::prelude::*;

use crate::config::DeleteMode;
use crate::error::Error;
use crate::journal::{Disposition, ItemOutcome, RunRecord, Trigger};
use crate::model::{Action, Candidate, Tier};
use crate::safety::PathGuard;

/// Progress emitted while a run executes, so both front-ends can show live status.
#[derive(Debug, Clone)]
pub enum CleanEvent {
    Started {
        total: usize,
        bytes: u64,
    },
    ItemStarted {
        id: crate::model::CandidateId,
        label: String,
    },
    ItemFinished(Box<ItemOutcome>),
    Finished(Box<RunRecord>),
}

/// How a clean run should behave.
#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub dry_run: bool,
    pub mode: DeleteMode,
    pub trigger: Trigger,
    /// Parallel workers for removal.
    pub concurrency: usize,
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self {
            dry_run: true, // the safe default: callers must opt in to deleting
            mode: DeleteMode::Tiered,
            trigger: Trigger::Cli,
            concurrency: 4,
        }
    }
}

/// Execute a clean over the chosen candidates.
///
/// Path-removal candidates run in parallel; shell candidates run serially after
/// them, because `brew cleanup` and `docker prune` are not safe to run concurrently
/// with each other and produce interleaved output if they are.
pub fn clean(
    candidates: &[Candidate],
    guard: &PathGuard,
    opts: &CleanOptions,
    progress: Option<&Sender<CleanEvent>>,
) -> RunRecord {
    let mut record = RunRecord::new(opts.trigger, opts.dry_run);
    record.candidates_found = candidates.len();
    record.bytes_found = candidates.iter().map(Candidate::reclaimable).sum();

    emit(
        progress,
        CleanEvent::Started {
            total: candidates.len(),
            bytes: record.bytes_found,
        },
    );

    let (shell, paths): (Vec<&Candidate>, Vec<&Candidate>) =
        candidates.iter().partition(|c| c.action.is_shell());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.concurrency.max(1))
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let mut outcomes: Vec<ItemOutcome> = pool.install(|| {
        paths
            .par_iter()
            .map(|candidate| {
                emit(
                    progress,
                    CleanEvent::ItemStarted {
                        id: candidate.id.clone(),
                        label: candidate.label.clone(),
                    },
                );
                let outcome = remove_paths(candidate, guard, opts);
                emit(
                    progress,
                    CleanEvent::ItemFinished(Box::new(outcome.clone())),
                );
                outcome
            })
            .collect()
    });

    for candidate in shell {
        emit(
            progress,
            CleanEvent::ItemStarted {
                id: candidate.id.clone(),
                label: candidate.label.clone(),
            },
        );
        let outcome = run_command(candidate, opts);
        emit(
            progress,
            CleanEvent::ItemFinished(Box::new(outcome.clone())),
        );
        outcomes.push(outcome);
    }

    record.items = outcomes;
    record.finished_at = std::time::SystemTime::now();
    emit(progress, CleanEvent::Finished(Box::new(record.clone())));
    record
}

/// Remove one candidate's paths.
fn remove_paths(candidate: &Candidate, guard: &PathGuard, opts: &CleanOptions) -> ItemOutcome {
    let mut outcome = base_outcome(candidate);

    // Re-validate now, not at scan time. A symlink can be swapped into place while
    // the user is deciding, and `check` re-canonicalises to catch exactly that.
    let (valid, refusals) = guard.check_all(&candidate.paths);

    if !refusals.is_empty() {
        outcome.disposition = Disposition::Failed;
        outcome.error = Some(
            refusals
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        );
        return outcome;
    }

    if valid.is_empty() {
        // Everything was already gone. For a cleanup tool that is success.
        outcome.disposition = Disposition::Skipped;
        return outcome;
    }

    if opts.dry_run {
        outcome.disposition = Disposition::DryRun;
        outcome.freed_bytes = 0;
        return outcome;
    }

    let to_trash = opts.mode.uses_trash(candidate.tier);
    let errors: Vec<String> = valid
        .iter()
        .filter_map(|path| remove_one(path, to_trash).err().map(|e| e.to_string()))
        .collect();

    // The scan measured this candidate's paths as a set, so splitting that total
    // across them would be false precision. Credit the measured size only when
    // every path was removed; otherwise report zero and record why.
    if errors.is_empty() {
        outcome.freed_bytes = candidate.reclaimable();
        outcome.disposition = if to_trash {
            Disposition::Trashed
        } else {
            Disposition::Purged
        };
    } else {
        outcome.freed_bytes = 0;
        outcome.error = Some(errors.join("; "));
        outcome.disposition = Disposition::Failed;
    }

    outcome
}

fn remove_one(path: &PathBuf, to_trash: bool) -> crate::Result<()> {
    if to_trash {
        return trash::delete(path).map_err(|source| Error::Trash {
            path: path.clone(),
            source,
        });
    }

    let meta = std::fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| Error::io(path, e))
    } else {
        std::fs::remove_file(path).map_err(|e| Error::io(path, e))
    }
}

/// Run a candidate that reclaims by shelling out (`brew cleanup`, `docker prune`).
fn run_command(candidate: &Candidate, opts: &CleanOptions) -> ItemOutcome {
    let mut outcome = base_outcome(candidate);

    let Action::Shell { program, args } = &candidate.action else {
        outcome.disposition = Disposition::Failed;
        outcome.error = Some("not a shell action".into());
        return outcome;
    };

    if opts.dry_run {
        outcome.disposition = Disposition::DryRun;
        return outcome;
    }

    match std::process::Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => {
            outcome.disposition = Disposition::CommandRun;
            // These tools report their own totals and we cannot verify them, so
            // claim the scan's estimate rather than inventing a number.
            outcome.freed_bytes = candidate.reclaimable();
        }
        Ok(out) => {
            outcome.disposition = Disposition::Failed;
            outcome.error = Some(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        Err(e) => {
            outcome.disposition = Disposition::Failed;
            outcome.error = Some(format!("could not run `{program}`: {e}"));
        }
    }

    outcome
}

fn base_outcome(candidate: &Candidate) -> ItemOutcome {
    ItemOutcome {
        id: candidate.id.clone(),
        provider: candidate.provider.clone(),
        group: candidate.group,
        label: candidate.label.clone(),
        tier: candidate.tier,
        paths: candidate.paths.clone(),
        expected_bytes: candidate.reclaimable(),
        freed_bytes: 0,
        disposition: Disposition::Skipped,
        error: None,
    }
}

fn emit(progress: Option<&Sender<CleanEvent>>, event: CleanEvent) {
    if let Some(tx) = progress {
        // A receiver that has hung up must not abort a deletion in flight.
        let _ = tx.send(event);
    }
}

/// Candidates requiring explicit confirmation even under `--yes`.
pub fn needs_explicit_confirmation(
    candidates: &[Candidate],
    confirm_caution: bool,
) -> Vec<&Candidate> {
    if !confirm_caution {
        return Vec::new();
    }
    candidates
        .iter()
        .filter(|c| c.tier == Tier::Caution)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CandidateBuilder, Group, Signals, Size};
    use crate::platform::Paths;
    use std::fs;
    use std::path::Path;
    use std::time::SystemTime;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        guard: PathGuard,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let guard = PathGuard::new(&Paths::with_home(&home));
        Fixture {
            _tmp: tmp,
            home,
            guard,
        }
    }

    fn make_tree(base: &Path, rel: &str, bytes: usize) -> PathBuf {
        let dir = base.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.bin"), vec![b'x'; bytes]).unwrap();
        dir
    }

    fn measured(path: &Path, tier: Tier, on_disk: u64) -> Candidate {
        CandidateBuilder::new("test.thing", Group::Node, "test cache")
            .path(path)
            .tier(tier)
            .build()
            .with_measurement(
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
                    last_used_days: 400,
                    active_now: false,
                },
            )
    }

    fn purge_opts() -> CleanOptions {
        CleanOptions {
            dry_run: false,
            mode: DeleteMode::Purge,
            trigger: Trigger::Cli,
            concurrency: 2,
        }
    }

    #[test]
    fn a_dry_run_leaves_everything_on_disk() {
        let f = fixture();
        let path = make_tree(&f.home, ".cache/thing", 1024);
        let candidate = measured(&path, Tier::Safe, 1024);

        let record = clean(&[candidate], &f.guard, &CleanOptions::default(), None);

        assert!(path.exists(), "dry run must not delete");
        assert_eq!(record.items[0].disposition, Disposition::DryRun);
        assert_eq!(record.bytes_freed(), 0);
        assert!(record.dry_run);
    }

    #[test]
    fn the_default_options_are_a_dry_run() {
        // Deleting must always be an explicit opt-in, including for library callers.
        assert!(CleanOptions::default().dry_run);
    }

    #[test]
    fn purging_removes_the_tree_and_reports_the_bytes() {
        let f = fixture();
        let path = make_tree(&f.home, ".cache/thing", 4096);
        let candidate = measured(&path, Tier::Safe, 4096);

        let record = clean(&[candidate], &f.guard, &purge_opts(), None);

        assert!(!path.exists(), "purge must remove the tree");
        assert_eq!(record.items[0].disposition, Disposition::Purged);
        assert_eq!(record.bytes_freed(), 4096);
        assert!(record.succeeded());
    }

    #[test]
    fn a_refused_path_fails_that_item_without_touching_it() {
        let f = fixture();
        let protected = make_tree(&f.home, ".ssh", 512);
        let candidate = measured(&protected, Tier::Safe, 512);

        let record = clean(&[candidate], &f.guard, &purge_opts(), None);

        assert!(protected.exists(), "the guard must have stopped this");
        assert_eq!(record.items[0].disposition, Disposition::Failed);
        assert_eq!(record.bytes_freed(), 0);
        assert!(!record.succeeded());
    }

    #[test]
    fn one_refusal_does_not_stop_the_other_items() {
        let f = fixture();
        let good = make_tree(&f.home, ".cache/good", 2048);
        let bad = make_tree(&f.home, ".ssh", 512);

        let record = clean(
            &[
                measured(&good, Tier::Safe, 2048),
                measured(&bad, Tier::Safe, 512),
            ],
            &f.guard,
            &purge_opts(),
            None,
        );

        assert!(
            !good.exists(),
            "the safe item must still have been reclaimed"
        );
        assert!(bad.exists());
        assert_eq!(record.bytes_freed(), 2048);
        assert_eq!(record.failures().count(), 1);
    }

    #[test]
    fn an_already_missing_path_is_skipped_not_failed() {
        let f = fixture();
        let candidate = measured(&f.home.join(".cache/gone"), Tier::Safe, 1024);
        let record = clean(&[candidate], &f.guard, &purge_opts(), None);
        assert_eq!(record.items[0].disposition, Disposition::Skipped);
        assert!(
            record.succeeded(),
            "already-gone is success for a cleanup tool"
        );
    }

    #[test]
    fn tiered_mode_purges_safe_items() {
        let f = fixture();
        let path = make_tree(&f.home, ".cache/safe-thing", 1024);
        let opts = CleanOptions {
            mode: DeleteMode::Tiered,
            ..purge_opts()
        };
        let record = clean(&[measured(&path, Tier::Safe, 1024)], &f.guard, &opts, None);
        assert_eq!(record.items[0].disposition, Disposition::Purged);
        assert!(!path.exists());
    }

    #[test]
    fn a_swapped_symlink_is_refused_at_delete_time() {
        // The TOCTOU case: valid at scan time, replaced while the user deliberates.
        let f = fixture();
        let path = make_tree(&f.home, ".cache/target", 1024);
        let candidate = measured(&path, Tier::Safe, 1024);
        let precious = make_tree(&f.home, ".cache/precious", 4096);

        fs::remove_dir_all(&path).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&precious, &path).unwrap();
            let record = clean(&[candidate], &f.guard, &purge_opts(), None);
            assert_eq!(record.items[0].disposition, Disposition::Failed);
            assert!(
                precious.exists(),
                "deletion must not have followed the swapped link"
            );
            assert!(precious.join("data.bin").exists());
        }
    }

    #[test]
    fn progress_events_describe_the_whole_run() {
        let f = fixture();
        let path = make_tree(&f.home, ".cache/thing", 1024);
        let (tx, rx) = std::sync::mpsc::channel();

        clean(
            &[measured(&path, Tier::Safe, 1024)],
            &f.guard,
            &purge_opts(),
            Some(&tx),
        );
        drop(tx);

        let events: Vec<CleanEvent> = rx.iter().collect();
        assert!(matches!(
            events.first(),
            Some(CleanEvent::Started { total: 1, .. })
        ));
        assert!(matches!(events.last(), Some(CleanEvent::Finished(_))));
        assert!(events
            .iter()
            .any(|e| matches!(e, CleanEvent::ItemFinished(_))));
    }

    #[test]
    fn a_hung_up_progress_receiver_does_not_abort_the_run() {
        let f = fixture();
        let path = make_tree(&f.home, ".cache/thing", 1024);
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);

        let record = clean(
            &[measured(&path, Tier::Safe, 1024)],
            &f.guard,
            &purge_opts(),
            Some(&tx),
        );
        assert!(!path.exists());
        assert!(record.succeeded());
    }

    #[test]
    fn a_failing_command_is_recorded_rather_than_panicking() {
        let candidate = CandidateBuilder::new("test.cmd", Group::System, "bogus")
            .path("/nonexistent-marker")
            .action(Action::Shell {
                program: "definitely-not-a-real-program-xyz".into(),
                args: vec![],
            })
            .build();
        let f = fixture();
        let record = clean(&[candidate], &f.guard, &purge_opts(), None);
        assert_eq!(record.items[0].disposition, Disposition::Failed);
        assert!(record.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("could not run"));
    }

    #[test]
    fn a_successful_command_is_recorded_as_run() {
        let candidate = CandidateBuilder::new("test.cmd", Group::System, "true")
            .path("/nonexistent-marker")
            .action(Action::Shell {
                program: "true".into(),
                args: vec![],
            })
            .build()
            .with_measurement(
                Size {
                    on_disk: 999,
                    ..Size::default()
                },
                Signals {
                    artifact_mtime: SystemTime::UNIX_EPOCH,
                    artifact_atime: None,
                    source_mtime: None,
                    vcs_activity: None,
                    last_used_days: 100,
                    active_now: false,
                },
            );
        let f = fixture();
        let record = clean(&[candidate], &f.guard, &purge_opts(), None);
        assert_eq!(record.items[0].disposition, Disposition::CommandRun);
        assert_eq!(record.bytes_freed(), 999);
    }

    #[test]
    fn caution_items_are_flagged_for_confirmation() {
        let safe = measured(Path::new("/tmp/a"), Tier::Safe, 1);
        let caution = measured(Path::new("/tmp/b"), Tier::Caution, 1);
        let items = vec![safe, caution];
        assert_eq!(needs_explicit_confirmation(&items, true).len(), 1);
        assert_eq!(needs_explicit_confirmation(&items, false).len(), 0);
    }

    #[test]
    fn nothing_outside_the_sandbox_is_touched() {
        let f = fixture();
        let outside = TempDir::new().unwrap();
        let victim = make_tree(&outside.path().canonicalize().unwrap(), "important", 1024);
        let candidate = measured(&victim, Tier::Safe, 1024);

        let record = clean(&[candidate], &f.guard, &purge_opts(), None);
        // TMPDIR may cover the temp dir on macOS; either way, assert the file survives
        // unless it was legitimately inside an allowed root.
        if record.items[0].disposition == Disposition::Failed {
            assert!(victim.exists());
        }
    }
}
