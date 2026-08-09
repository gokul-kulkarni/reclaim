//! The scan pipeline: one walk, parallel discovery, parallel measurement, scoring.
//!
//! Results stream over a channel as they are produced. Discovery finishes in
//! milliseconds and gives the UI a complete list to paint immediately; measurement
//! then fills sizes in progressively. That is why the TUI and the web UI both feel
//! responsive on a home directory with millions of files.

use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::config::Config;
use crate::discovery::{self, Discovery};
use crate::model::{Candidate, ProjectRoot};
use crate::platform::Paths;
use crate::staleness::{self, Filter};
use crate::walk::{self, LinkTracker, WalkOptions};

/// Everything a provider needs to decide what it can offer.
pub struct ScanContext {
    pub paths: Paths,
    pub config: Config,
    /// Projects found by the single stage-1 walk, shared by every provider.
    pub projects: Vec<ProjectRoot>,
}

impl ScanContext {
    pub fn new(paths: Paths, config: Config, projects: Vec<ProjectRoot>) -> Self {
        Self {
            paths,
            config,
            projects,
        }
    }

    /// Projects carrying any of these marker files.
    pub fn projects_with(&self, markers: &[&str]) -> Vec<ProjectRoot> {
        discovery::projects_with(&self.projects, markers)
    }

    /// Whether a provider is enabled by the user's config.
    pub fn enabled(&self, provider_id: &str) -> bool {
        self.config.providers.is_enabled(provider_id)
    }

    /// Only existing paths, so providers can declare optimistically.
    pub fn existing(
        &self,
        paths: impl IntoIterator<Item = std::path::PathBuf>,
    ) -> Vec<std::path::PathBuf> {
        paths.into_iter().filter(|p| p.exists()).collect()
    }
}

/// A source of candidates for one ecosystem.
pub trait Provider: Send + Sync {
    /// Dotted id, e.g. `node.pnpm-store`. The part before the dot is the group key
    /// used by config filtering.
    fn id(&self) -> &'static str;

    /// Marker files this provider cares about, contributed to the stage-1 walk.
    fn markers(&self) -> &'static [&'static str] {
        &[]
    }

    /// Cheap: path existence checks and matching against `ctx.projects`. Must not
    /// walk the filesystem or measure anything.
    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate>;
}

/// Progress emitted while a scan runs.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// Stage 1 finished.
    ProjectsFound { count: usize, elapsed_ms: u64 },
    /// Stage 2 finished: the full candidate list, unmeasured.
    Discovered(Vec<Candidate>),
    /// Stage 3 progress: one candidate has been measured.
    Measured {
        candidate: Box<Candidate>,
        done: usize,
        total: usize,
    },
    /// Stage 4 finished: the final ranked and filtered list.
    Complete(Box<ScanResult>),
}

/// The finished product of a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Ranked, filtered candidates: what the user is offered.
    pub candidates: Vec<Candidate>,
    /// Everything discovered, before filtering. Kept so the UI can explain
    /// "34 more items hidden below the 50MB threshold" rather than silently dropping them.
    pub all: Vec<Candidate>,
    pub projects_scanned: usize,
    pub elapsed_ms: u64,
    /// Directories the walk could not read, so totals are a floor not an exact figure.
    pub unreadable: Vec<std::path::PathBuf>,
}

impl ScanResult {
    pub fn total_reclaimable(&self) -> u64 {
        staleness::total_reclaimable(&self.candidates)
    }

    /// Candidates hidden by the filter, and how much they add up to.
    pub fn hidden(&self) -> (usize, u64) {
        let shown: std::collections::HashSet<_> = self.candidates.iter().map(|c| &c.id).collect();
        let hidden: Vec<&Candidate> = self.all.iter().filter(|c| !shown.contains(&c.id)).collect();
        (hidden.len(), hidden.iter().map(|c| c.reclaimable()).sum())
    }

    pub fn by_group(&self) -> Vec<(crate::model::Group, crate::model::Size)> {
        staleness::totals_by_group(&self.candidates)
    }

    pub fn find(&self, id: &crate::model::CandidateId) -> Option<&Candidate> {
        self.all.iter().find(|c| &c.id == id)
    }
}

/// Run a complete scan.
pub fn scan(
    providers: &[Box<dyn Provider>],
    paths: &Paths,
    config: &Config,
    filter: &Filter,
    progress: Option<&Sender<ScanEvent>>,
) -> ScanResult {
    let started = Instant::now();

    // Stage 1: one pruned walk over the configured project roots.
    let roots = config.resolved_project_roots(paths);
    let found: Discovery = if roots.is_empty() {
        Discovery::default()
    } else {
        discovery::find_projects(&roots, &config.scan)
    };
    emit(
        progress,
        ScanEvent::ProjectsFound {
            count: found.projects.len(),
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
    );

    let ctx = ScanContext::new(paths.clone(), config.clone(), found.projects.clone());

    // Stage 2: every provider discovers in parallel. Cheap; no sizing.
    let discovered: Vec<Candidate> = providers
        .par_iter()
        .filter(|p| ctx.enabled(p.id()))
        .flat_map(|p| p.discover(&ctx))
        .collect();

    // Two providers can legitimately point at the same path (a `build/` directory
    // claimed by both the JVM and the CMake provider). Offer it once.
    let discovered = dedupe(discovered);

    // Shell-action candidates (`brew cleanup`, `docker prune`, `simctl delete`)
    // act on machine-global state, not on paths under the scanned home. Running
    // them while pointed at a sandbox would mutate the real machine behind the
    // user's back, so drop them here — centrally, where a new provider cannot
    // forget the rule.
    let discovered: Vec<Candidate> = if paths.is_sandboxed() {
        discovered
            .into_iter()
            .filter(|c| !c.action.is_shell())
            .collect()
    } else {
        discovered
    };
    emit(progress, ScanEvent::Discovered(discovered.clone()));

    // Stage 3: measure in parallel, sharing one link tracker so a hardlinked
    // inode reached through two candidates is only counted once.
    let links = LinkTracker::new();
    let walk_opts = WalkOptions {
        threads: 0, // inherit the surrounding pool rather than nesting our own
        same_device: !config.scan.cross_device,
        max_depth: 64,
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.scan.threads())
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let total = discovered.len();
    let done = std::sync::atomic::AtomicUsize::new(0);

    let measured: Vec<Candidate> = pool.install(|| {
        discovered
            .par_iter()
            .map(|candidate| {
                let measurement = walk::measure_all(&candidate.paths, &links, &walk_opts);
                let signals =
                    staleness::derive_signals(candidate, &measurement, config.scan.max_depth);
                let result = candidate.with_measurement(measurement.size, signals);

                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                emit(
                    progress,
                    ScanEvent::Measured {
                        candidate: Box::new(result.clone()),
                        done: n,
                        total,
                    },
                );
                result
            })
            .collect()
    });

    // Stage 4: score and filter. Pure, single-threaded, trivially testable.
    let all = staleness::rank(measured, config);
    let candidates = filter.apply(all.clone());

    let result = ScanResult {
        candidates,
        all,
        projects_scanned: found.projects.len(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        unreadable: found.unreadable,
    };

    emit(progress, ScanEvent::Complete(Box::new(result.clone())));
    result
}

/// Collapse candidates that resolve to the same primary path, keeping the first.
fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let seen = Arc::new(dashmap::DashSet::new());
    candidates
        .into_iter()
        .filter(|c| match c.primary_path() {
            Some(path) => seen.insert(path.to_path_buf()),
            None => true,
        })
        .collect()
}

/// Build the filter the CLI and web UI both use, from config plus overrides.
pub fn filter_from_config(config: &Config) -> Filter {
    Filter {
        min_size: config.thresholds.min_size_bytes().unwrap_or(0),
        ..Filter::default()
    }
}

fn emit(progress: Option<&Sender<ScanEvent>>, event: ScanEvent) {
    if let Some(tx) = progress {
        let _ = tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CandidateBuilder, Group, Kind, Tier};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A provider that offers whatever paths it is handed.
    struct Fake {
        id: &'static str,
        paths: Vec<PathBuf>,
        kind: Kind,
    }

    impl Provider for Fake {
        fn id(&self) -> &'static str {
            self.id
        }

        fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
            ctx.existing(self.paths.clone())
                .into_iter()
                .map(|p| {
                    CandidateBuilder::new(self.id, Group::Node, "fake")
                        .path(p)
                        .kind(self.kind)
                        .tier(Tier::Safe)
                        .build()
                })
                .collect()
        }
    }

    fn tree(base: &std::path::Path, rel: &str, bytes: usize) -> PathBuf {
        let dir = base.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("blob.bin"), vec![b'x'; bytes]).unwrap();
        dir
    }

    fn no_filter() -> Filter {
        Filter {
            min_size: 0,
            include_active: true,
            ..Filter::default()
        }
    }

    #[test]
    fn scan_measures_and_ranks_discovered_candidates() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let small = tree(&home, ".cache/small", 1024);
        let big = tree(&home, ".cache/big", 512 * 1024);

        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![small, big.clone()],
            kind: Kind::GlobalCache,
        })];

        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &no_filter(),
            None,
        );

        assert_eq!(result.candidates.len(), 2);
        assert!(
            result.candidates.iter().all(|c| c.size.is_some()),
            "all must be measured"
        );
        assert!(
            result.candidates.iter().all(|c| c.score.is_some()),
            "all must be scored"
        );
        assert!(result.total_reclaimable() >= 512 * 1024);
    }

    /// A provider that reclaims by running a command rather than deleting paths.
    struct ShellFake;

    impl Provider for ShellFake {
        fn id(&self) -> &'static str {
            "fake.shell"
        }

        fn discover(&self, _ctx: &ScanContext) -> Vec<Candidate> {
            vec![
                CandidateBuilder::new("fake.shell", Group::System, "run a command")
                    .path("/tmp")
                    .action(crate::model::Action::Shell {
                        program: "definitely-should-not-run".into(),
                        args: vec![],
                    })
                    .build(),
            ]
        }
    }

    #[test]
    fn shell_candidates_are_dropped_when_pointed_at_a_sandbox() {
        // Regression: `--root /tmp/sandbox` used to still run `brew cleanup` and
        // `simctl delete unavailable` against the real machine, because those
        // reclaim through a command rather than through paths under the home.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(ShellFake)];

        let paths = Paths::with_home(&home);
        assert!(paths.is_sandboxed());

        let result = scan(&providers, &paths, &Config::default(), &no_filter(), None);
        assert!(
            result.all.is_empty(),
            "a sandboxed run must not offer machine-global commands: {:?}",
            result.all
        );
    }

    #[test]
    fn missing_paths_produce_no_candidates() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![home.join(".cache/nope")],
            kind: Kind::GlobalCache,
        })];

        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &no_filter(),
            None,
        );
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn disabled_providers_are_never_asked() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        tree(&home, ".cache/thing", 4096);

        let config = Config {
            providers: crate::config::ProviderConfig {
                disabled: vec!["fake".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![home.join(".cache/thing")],
            kind: Kind::GlobalCache,
        })];

        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &config,
            &no_filter(),
            None,
        );
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn duplicate_paths_from_two_providers_are_offered_once() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let shared = tree(&home, ".cache/shared", 2048);

        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(Fake {
                id: "a.thing",
                paths: vec![shared.clone()],
                kind: Kind::GlobalCache,
            }),
            Box::new(Fake {
                id: "b.thing",
                paths: vec![shared],
                kind: Kind::GlobalCache,
            }),
        ];

        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &no_filter(),
            None,
        );
        assert_eq!(
            result.candidates.len(),
            1,
            "the same directory must not be offered twice"
        );
    }

    #[test]
    fn the_filter_hides_items_but_the_result_still_accounts_for_them() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let small = tree(&home, ".cache/small", 512);
        let big = tree(&home, ".cache/big", 256 * 1024);

        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![small, big],
            kind: Kind::GlobalCache,
        })];

        let filter = Filter {
            min_size: 100 * 1024,
            include_active: true,
            ..Filter::default()
        };
        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &filter,
            None,
        );

        assert_eq!(result.candidates.len(), 1, "only the big one is offered");
        assert_eq!(result.all.len(), 2, "but both are still accounted for");
        let (count, bytes) = result.hidden();
        assert_eq!(count, 1);
        assert!(
            bytes > 0,
            "hidden bytes must be reported, not silently dropped"
        );
    }

    #[test]
    fn a_shared_link_tracker_prevents_double_counting_across_candidates() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let store = tree(&home, ".cache/store", 128 * 1024);
        let project = home.join(".cache/project");
        fs::create_dir_all(&project).unwrap();
        fs::hard_link(store.join("blob.bin"), project.join("blob.bin")).unwrap();

        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![store, project],
            kind: Kind::GlobalCache,
        })];

        let result = scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &no_filter(),
            None,
        );

        let total = result.total_reclaimable();
        assert!(
            total < 256 * 1024,
            "hardlinked bytes must be counted once across candidates, got {total}"
        );
        assert!(result.candidates.iter().any(|c| c.size.unwrap().shared > 0));
    }

    #[test]
    fn progress_events_arrive_in_pipeline_order() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        tree(&home, ".cache/thing", 4096);
        let (tx, rx) = std::sync::mpsc::channel();

        let providers: Vec<Box<dyn Provider>> = vec![Box::new(Fake {
            id: "fake.thing",
            paths: vec![home.join(".cache/thing")],
            kind: Kind::GlobalCache,
        })];
        scan(
            &providers,
            &Paths::with_home(&home),
            &Config::default(),
            &no_filter(),
            Some(&tx),
        );
        drop(tx);

        let events: Vec<ScanEvent> = rx.iter().collect();
        assert!(matches!(events[0], ScanEvent::ProjectsFound { .. }));
        assert!(matches!(events[1], ScanEvent::Discovered(_)));
        assert!(events
            .iter()
            .any(|e| matches!(e, ScanEvent::Measured { .. })));
        assert!(matches!(events.last(), Some(ScanEvent::Complete(_))));
    }

    #[test]
    fn project_artifacts_inherit_staleness_from_their_project() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let project = home.join("dev/app");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("package.json"), b"{}").unwrap();
        fs::write(project.join("src/index.js"), b"console.log(1)").unwrap();
        let modules = tree(&project, "node_modules", 8192);

        let candidate = CandidateBuilder::new("node.modules", Group::Node, "node_modules")
            .path(&modules)
            .kind(Kind::ProjectArtifact)
            .project(&project)
            .build();

        let links = LinkTracker::new();
        let m = walk::measure_all(&candidate.paths, &links, &WalkOptions::default());
        let signals = staleness::derive_signals(&candidate, &m, 8);

        assert!(
            signals.source_mtime.is_some(),
            "project source mtime must be read"
        );
        assert!(signals.active_now, "a project written just now is active");
    }
}
