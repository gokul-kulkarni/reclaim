//! Shared wiring: resolving config, paths and filters once, so every subcommand
//! and both front-ends see exactly the same view of the machine.

use anyhow::{Context, Result};
use reclaim_core::config::Config;
use reclaim_core::journal::Journal;
use reclaim_core::model::Tier;
use reclaim_core::pipeline::{self, Provider, ScanResult};
use reclaim_core::staleness::Filter;
use reclaim_core::{PathGuard, Paths};

use crate::cli::{FilterArgs, GlobalArgs};

/// What a filter is being built for. Reporting and deleting want different
/// defaults, and conflating them either hides space from the user or deletes
/// something a build is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// `scan`, the TUI, the web UI: apply the size threshold, show active items.
    Report,
    /// `scan --all`: no size threshold either.
    ShowEverything,
    /// `clean`: apply the size threshold and exclude actively-used items.
    Delete,
}

impl Purpose {
    fn is_reporting(self) -> bool {
        matches!(self, Purpose::Report | Purpose::ShowEverything)
    }
}

/// Everything a subcommand needs, resolved once.
pub struct App {
    pub paths: Paths,
    pub config: Config,
    pub guard: PathGuard,
    pub journal: Journal,
    pub providers: Vec<Box<dyn Provider>>,
}

impl App {
    pub fn new(global: &GlobalArgs) -> Result<Self> {
        // `--root` makes the whole engine operate on a different home, which is
        // what makes end-to-end tests possible without touching the real one.
        let paths = match &global.root {
            Some(root) => {
                let canonical = root
                    .canonicalize()
                    .with_context(|| format!("--root {} does not exist", root.display()))?;
                Paths::with_home(canonical)
            }
            None => Paths::from_env().context("could not determine your home directory")?,
        };

        let config_path = global.config.clone().unwrap_or_else(|| paths.config_file());
        let mut config = Config::load_from(&config_path, &paths)
            .with_context(|| format!("loading config from {}", config_path.display()))?;

        if let Some(concurrency) = global.concurrency {
            config.scan.concurrency = concurrency;
        }

        let guard = PathGuard::new(&paths).protect(config.resolved_protected_paths(&paths));
        let journal = Journal::new(paths.journal_dir());

        Ok(Self {
            paths,
            config,
            guard,
            journal,
            providers: reclaim_providers::all(),
        })
    }

    /// Run a full scan with the given filter.
    pub fn scan(&self, filter: &Filter) -> ScanResult {
        pipeline::scan(&self.providers, &self.paths, &self.config, filter, None)
    }

    /// Build a filter from config defaults plus command-line overrides.
    pub fn filter(&self, args: &FilterArgs, purpose: Purpose) -> Result<Filter> {
        let configured_min = self.config.thresholds.min_size_bytes()?;

        Ok(Filter {
            min_size: if purpose == Purpose::ShowEverything {
                0
            } else {
                args.min_size.unwrap_or(configured_min)
            },
            max_tier: args.tier,
            // An explicit --older-than wins; otherwise, when a tier is named, use
            // that tier's configured threshold, since "safe" and "caution" warrant
            // very different waiting periods.
            min_age_days: args.older_than.or_else(|| {
                args.tier
                    .map(|tier| self.config.thresholds.per_tier.for_tier(tier))
            }),
            groups: (!args.group.is_empty()).then(|| args.group.clone()),
            providers: (!args.provider.is_empty()).then(|| args.provider.clone()),
            // Reporting shows everything, including things touched in the last
            // 24h: a tool for finding disk space must never hide space from you.
            // Deleting excludes them, because removing a cache a build is
            // currently writing to produces confusing failures. The scoring
            // already ranks them near the bottom either way.
            include_active: args.include_active || purpose.is_reporting(),
        })
    }

    /// The tier ceiling a `clean` run may touch when the user did not name one.
    ///
    /// Defaults to Safe: an unqualified `reclaim clean --yes` must never be able
    /// to remove something irreplaceable.
    pub fn effective_tier(&self, requested: Option<Tier>) -> Tier {
        requested.unwrap_or(Tier::Safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;

    fn global(root: &std::path::Path) -> GlobalArgs {
        GlobalArgs {
            config: None,
            root: Some(root.to_path_buf()),
            concurrency: Some(2),
            verbose: 0,
            no_color: true,
        }
    }

    #[test]
    fn a_root_override_scopes_everything_to_that_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();
        let canonical = tmp.path().canonicalize().unwrap();

        assert_eq!(app.paths.home(), canonical.as_path());
        assert!(app.journal.dir().starts_with(&canonical));
        assert!(app.guard.home().starts_with(&canonical));
    }

    #[test]
    fn a_missing_root_is_a_clear_error_rather_than_a_panic() {
        let result = App::new(&global(std::path::Path::new("/definitely/not/here")));
        let err = result
            .err()
            .expect("a missing root must be an error")
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn clean_defaults_to_the_safe_tier_only() {
        // `reclaim clean --yes` with no tier must not be able to touch Caution items.
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();
        assert_eq!(app.effective_tier(None), Tier::Safe);
        assert_eq!(app.effective_tier(Some(Tier::Caution)), Tier::Caution);
    }

    #[test]
    fn naming_a_tier_applies_that_tiers_configured_age_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();

        let safe = app
            .filter(
                &FilterArgs {
                    tier: Some(Tier::Safe),
                    ..Default::default()
                },
                Purpose::Delete,
            )
            .unwrap();
        let caution = app
            .filter(
                &FilterArgs {
                    tier: Some(Tier::Caution),
                    ..Default::default()
                },
                Purpose::Delete,
            )
            .unwrap();

        assert_eq!(safe.min_age_days, Some(14));
        assert_eq!(
            caution.min_age_days,
            Some(180),
            "riskier items wait longer by default"
        );
    }

    #[test]
    fn an_explicit_age_overrides_the_tier_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();

        let filter = app
            .filter(
                &FilterArgs {
                    tier: Some(Tier::Safe),
                    older_than: Some(365),
                    ..Default::default()
                },
                Purpose::Delete,
            )
            .unwrap();
        assert_eq!(filter.min_age_days, Some(365));
    }

    #[test]
    fn the_all_flag_drops_the_size_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();

        let normal = app.filter(&FilterArgs::default(), Purpose::Report).unwrap();
        let all = app
            .filter(&FilterArgs::default(), Purpose::ShowEverything)
            .unwrap();
        assert!(normal.min_size > 0);
        assert_eq!(all.min_size, 0);
    }

    #[test]
    fn reporting_shows_actively_used_items_but_deleting_does_not() {
        // A tool for finding disk space must never hide space; a tool for
        // deleting must not touch what a build is writing to right now.
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();

        assert!(
            app.filter(&FilterArgs::default(), Purpose::Report)
                .unwrap()
                .include_active
        );
        assert!(
            app.filter(&FilterArgs::default(), Purpose::ShowEverything)
                .unwrap()
                .include_active
        );
        assert!(
            !app.filter(&FilterArgs::default(), Purpose::Delete)
                .unwrap()
                .include_active
        );
    }

    #[test]
    fn the_include_active_flag_overrides_the_delete_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();
        let filter = app
            .filter(
                &FilterArgs {
                    include_active: true,
                    ..Default::default()
                },
                Purpose::Delete,
            )
            .unwrap();
        assert!(filter.include_active);
    }

    #[test]
    fn scanning_an_empty_home_finds_nothing_and_does_not_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(&global(tmp.path())).unwrap();
        let filter = app
            .filter(&FilterArgs::default(), Purpose::ShowEverything)
            .unwrap();
        let result = app.scan(&filter);
        assert_eq!(result.total_reclaimable(), 0);
    }
}
