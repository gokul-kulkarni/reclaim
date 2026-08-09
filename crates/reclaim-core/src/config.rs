//! User configuration.
//!
//! Layering, lowest precedence first: built-in defaults, then the TOML file, then
//! `RECLAIM_*` environment variables, then command-line flags (applied by the CLI).
//! Every field has a working default, so the tool runs correctly with no config file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::format;
use crate::model::Tier;
use crate::platform::Paths;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scan: ScanConfig,
    pub thresholds: ThresholdConfig,
    pub delete: DeleteConfig,
    pub providers: ProviderConfig,
    pub ui: UiConfig,
    pub schedule: ScheduleConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Directories crawled to find projects. Empty means "known caches only".
    pub project_roots: Vec<String>,
    /// Glob patterns pruned during the project walk.
    pub exclude: Vec<String>,
    /// How deep the project walk descends below each root.
    pub max_depth: usize,
    /// Worker threads. 0 means auto: `min(cpus * 2, 16)`.
    pub concurrency: usize,
    pub follow_symlinks: bool,
    /// Whether the walk may cross onto another filesystem.
    pub cross_device: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            project_roots: Vec::new(),
            exclude: vec![
                "**/Library/**".into(),
                "**/.Trash/**".into(),
                "**/.git/**".into(),
                "**/node_modules/**".into(),
                "**/.venv/**".into(),
                "**/Applications/**".into(),
            ],
            max_depth: 8,
            concurrency: 0,
            follow_symlinks: false,
            cross_device: false,
        }
    }
}

impl ScanConfig {
    /// Resolve `concurrency: 0` to a real thread count.
    ///
    /// Capped at 16 because this workload is metadata-bound: past that point the
    /// threads queue on the filesystem instead of doing useful work.
    pub fn threads(&self) -> usize {
        if self.concurrency > 0 {
            self.concurrency
        } else {
            (num_cpus::get() * 2).clamp(1, 16)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThresholdConfig {
    /// Candidates smaller than this are hidden, to keep the list readable.
    pub min_size: String,
    /// Baseline for the staleness factor in the reclaim score.
    pub stale_after_days: u32,
    pub per_tier: PerTierDays,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        Self {
            min_size: "50MB".into(),
            stale_after_days: 60,
            per_tier: PerTierDays::default(),
        }
    }
}

impl ThresholdConfig {
    pub fn min_size_bytes(&self) -> Result<u64> {
        format::parse_bytes(&self.min_size)
            .map_err(|e| Error::Config(format!("thresholds.min_size: {e}")))
    }
}

/// Default age threshold per tier: the riskier the item, the longer we wait.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerTierDays {
    pub safe: u32,
    pub review: u32,
    pub caution: u32,
}

impl Default for PerTierDays {
    fn default() -> Self {
        Self {
            safe: 14,
            review: 60,
            caution: 180,
        }
    }
}

impl PerTierDays {
    pub fn for_tier(&self, tier: Tier) -> u32 {
        match tier {
            Tier::Safe => self.safe,
            Tier::Review => self.review,
            Tier::Caution => self.caution,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteMode {
    /// Purge Safe items so space is freed immediately; Trash everything else.
    Tiered,
    /// Everything goes to the Trash. Recoverable, but frees nothing until emptied.
    Trash,
    /// Everything is removed permanently.
    Purge,
}

impl DeleteMode {
    /// Whether an item of this tier goes to the Trash rather than being unlinked.
    pub fn uses_trash(self, tier: Tier) -> bool {
        match self {
            DeleteMode::Trash => true,
            DeleteMode::Purge => false,
            DeleteMode::Tiered => tier != Tier::Safe,
        }
    }
}

impl std::str::FromStr for DeleteMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "tiered" => Ok(DeleteMode::Tiered),
            "trash" => Ok(DeleteMode::Trash),
            "purge" => Ok(DeleteMode::Purge),
            other => Err(format!(
                "unknown delete mode `{other}` (expected tiered|trash|purge)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeleteConfig {
    pub mode: DeleteMode,
    /// Prompt for Caution items even under `--yes`.
    pub confirm_caution: bool,
    /// Additive to the built-in protected list; nothing can subtract from that.
    pub protected_paths: Vec<String>,
}

impl Default for DeleteConfig {
    fn default() -> Self {
        Self {
            mode: DeleteMode::Tiered,
            confirm_caution: true,
            protected_paths: vec!["~/Documents".into(), "~/Desktop".into(), "~/.ssh".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// `["*"]` enables everything.
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub apple: AppleOptions,
    pub node: NodeOptions,
    pub containers: ContainerOptions,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: vec!["*".into()],
            // ML model caches are large but usually deliberate downloads, so they
            // are opt-in rather than offered by default.
            disabled: vec!["ml".into()],
            apple: AppleOptions::default(),
            node: NodeOptions::default(),
            containers: ContainerOptions::default(),
        }
    }
}

impl ProviderConfig {
    /// A provider is active if it is enabled and not disabled. Matching is by
    /// exact id or by the group prefix, so `node` disables all `node.*` providers.
    pub fn is_enabled(&self, provider_id: &str) -> bool {
        let matches = |patterns: &[String]| {
            patterns.iter().any(|p| {
                p == "*"
                    || p == provider_id
                    || provider_id
                        .strip_prefix(p)
                        .is_some_and(|r| r.starts_with('.'))
            })
        };
        matches(&self.enabled) && !matches(&self.disabled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppleOptions {
    /// Simulator runtimes to keep, newest first.
    pub keep_latest_simulator_runtimes: usize,
    /// Offer non-iOS runtimes (tvOS/watchOS/visionOS) for removal.
    pub offer_non_ios_runtimes: bool,
}

impl Default for AppleOptions {
    fn default() -> Self {
        Self {
            keep_latest_simulator_runtimes: 2,
            offer_non_ios_runtimes: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeOptions {
    /// Keep the pnpm content-addressable store out of the candidate list.
    ///
    /// Defaults to true: the store is hardlinked into every `node_modules` on the
    /// machine, so deleting it breaks working installs for a very small win.
    pub keep_pnpm_store: bool,
    /// Offer `node_modules` directories found in projects.
    pub offer_node_modules: bool,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            keep_pnpm_store: true,
            offer_node_modules: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerOptions {
    /// Include named volumes in the docker prune candidate.
    ///
    /// Off by default (`false` is the derived default): volumes hold database
    /// contents and other state that exists nowhere else.
    pub include_volumes: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// 0 picks a free port.
    pub port: u16,
    pub open_browser: bool,
    /// `auto` | `light` | `dark`
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            port: 0,
            open_browser: true,
            theme: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub enabled: bool,
    /// `weekly` | `biweekly` | `monthly`, or a five-field cron expression.
    pub cadence: String,
    /// Tiers a background run may touch. `caution` is rejected at load time.
    pub tiers: Vec<Tier>,
    pub older_than_days: u32,
    /// The first scheduled run only reports what it would have done.
    pub dry_run_first: bool,
    pub notify: bool,
    pub max_runtime_minutes: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cadence: "weekly".into(),
            tiers: vec![Tier::Safe],
            older_than_days: 60,
            dry_run_first: true,
            notify: true,
            max_runtime_minutes: 30,
        }
    }
}

impl Config {
    /// Load from the standard location, falling back to defaults if absent.
    pub fn load(paths: &Paths) -> Result<Self> {
        Self::load_from(&paths.config_file(), paths)
    }

    /// Load from an explicit file. A missing file yields defaults; an unreadable
    /// or malformed one is an error, because silently ignoring a typo'd config in
    /// a tool that deletes files is unacceptable.
    pub fn load_from(path: &Path, paths: &Paths) -> Result<Self> {
        let mut config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text).map_err(|source| Error::ConfigParse {
                path: path.to_path_buf(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(Error::io(path, e)),
        };
        config.apply_env();
        config.validate(paths)?;
        Ok(config)
    }

    /// Apply `RECLAIM_*` overrides. Only the settings worth scripting are exposed.
    fn apply_env(&mut self) {
        if let Some(v) = std::env::var_os("RECLAIM_CONCURRENCY") {
            if let Ok(n) = v.to_string_lossy().parse() {
                self.scan.concurrency = n;
            }
        }
        if let Some(v) = std::env::var_os("RECLAIM_MIN_SIZE") {
            self.thresholds.min_size = v.to_string_lossy().into_owned();
        }
        if let Some(v) = std::env::var_os("RECLAIM_DELETE_MODE") {
            if let Ok(mode) = v.to_string_lossy().parse() {
                self.delete.mode = mode;
            }
        }
        if let Some(v) = std::env::var_os("RECLAIM_PROJECT_ROOTS") {
            self.scan.project_roots = v
                .to_string_lossy()
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }

    /// Reject configurations that would misbehave later, at load time.
    pub fn validate(&self, _paths: &Paths) -> Result<()> {
        self.thresholds.min_size_bytes()?;

        if self.scan.max_depth == 0 {
            return Err(Error::Config("scan.max_depth must be at least 1".into()));
        }

        for pattern in &self.scan.exclude {
            glob::Pattern::new(pattern)
                .map_err(|e| Error::Config(format!("scan.exclude `{pattern}`: {e}")))?;
        }

        // A background job must never be able to remove something irreplaceable
        // while the user is not watching.
        if self.schedule.tiers.contains(&Tier::Caution) {
            return Err(Error::Config(
                "schedule.tiers may not include `caution`: background runs never \
                 remove irreplaceable data"
                    .into(),
            ));
        }

        if !["auto", "light", "dark"].contains(&self.ui.theme.as_str()) {
            return Err(Error::Config(format!(
                "ui.theme `{}` is not one of auto|light|dark",
                self.ui.theme
            )));
        }

        Ok(())
    }

    /// Resolved project roots, with `~` and `$VAR` expanded and duplicates removed.
    pub fn resolved_project_roots(&self, paths: &Paths) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        self.scan
            .project_roots
            .iter()
            .map(|raw| paths.expand(raw))
            .filter(|p| p.is_dir())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    pub fn resolved_protected_paths(&self, paths: &Paths) -> Vec<PathBuf> {
        self.delete
            .protected_paths
            .iter()
            .map(|raw| paths.expand(raw))
            .collect()
    }

    /// Serialise back to TOML, for `reclaim config init`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))
    }
}

/// The commented starter file written by `reclaim config init`.
pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../assets/config.default.toml");

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths() -> Paths {
        Paths::with_home("/home/tester")
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate(&paths()).unwrap();
    }

    #[test]
    fn a_missing_file_yields_defaults_rather_than_an_error() {
        let cfg = Config::load_from(Path::new("/nonexistent/reclaim.toml"), &paths()).unwrap();
        assert_eq!(cfg.thresholds.min_size, "50MB");
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        let err = Config::load_from(&path, &paths()).unwrap_err();
        assert!(matches!(err, Error::ConfigParse { .. }), "got {err:?}");
    }

    #[test]
    fn an_unknown_key_is_rejected_so_typos_are_not_silently_ignored() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[thresholds]\nmin_sise = \"1GB\"\n").unwrap();
        assert!(Config::load_from(&path, &paths()).is_err());
    }

    #[test]
    fn partial_files_keep_defaults_for_everything_else() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[thresholds]\nmin_size = \"1GB\"\n").unwrap();
        let cfg = Config::load_from(&path, &paths()).unwrap();
        assert_eq!(cfg.thresholds.min_size, "1GB");
        assert_eq!(
            cfg.thresholds.stale_after_days, 60,
            "untouched keys keep their defaults"
        );
        assert_eq!(cfg.delete.mode, DeleteMode::Tiered);
    }

    #[test]
    fn schedule_refuses_to_include_the_caution_tier() {
        let cfg = Config {
            schedule: ScheduleConfig {
                tiers: vec![Tier::Safe, Tier::Caution],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = cfg.validate(&paths()).unwrap_err().to_string();
        assert!(err.contains("caution"), "{err}");
    }

    #[test]
    fn an_invalid_min_size_is_caught_at_load_time() {
        let cfg = Config {
            thresholds: ThresholdConfig {
                min_size: "50 bananas".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.validate(&paths()).is_err());
    }

    #[test]
    fn tiered_mode_purges_safe_and_trashes_the_rest() {
        let m = DeleteMode::Tiered;
        assert!(
            !m.uses_trash(Tier::Safe),
            "safe items must free space immediately"
        );
        assert!(m.uses_trash(Tier::Review));
        assert!(m.uses_trash(Tier::Caution));
    }

    #[test]
    fn trash_and_purge_modes_are_absolute() {
        assert!(DeleteMode::Trash.uses_trash(Tier::Safe));
        assert!(!DeleteMode::Purge.uses_trash(Tier::Caution));
    }

    #[test]
    fn provider_filtering_matches_ids_and_group_prefixes() {
        let cfg = ProviderConfig {
            enabled: vec!["*".into()],
            disabled: vec!["node".into(), "apple.archives".into()],
            ..Default::default()
        };
        assert!(cfg.is_enabled("rust.target"));
        assert!(
            !cfg.is_enabled("node.npm-cache"),
            "group prefix disables all its providers"
        );
        assert!(!cfg.is_enabled("apple.archives"));
        assert!(cfg.is_enabled("apple.derived-data"));
    }

    #[test]
    fn provider_prefix_matching_does_not_catch_similar_names() {
        let cfg = ProviderConfig {
            disabled: vec!["node".into()],
            ..Default::default()
        };
        assert!(
            cfg.is_enabled("nodejs-extra.cache"),
            "`node` must not match `nodejs-extra`"
        );
    }

    #[test]
    fn threads_resolve_from_zero_and_stay_within_bounds() {
        let auto = ScanConfig::default().threads();
        assert!((1..=16).contains(&auto), "auto concurrency was {auto}");
        let explicit = ScanConfig {
            concurrency: 4,
            ..Default::default()
        }
        .threads();
        assert_eq!(explicit, 4);
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let original = Config::default();
        let text = original.to_toml().unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn the_shipped_template_parses_and_validates() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TEMPLATE)
            .expect("the template we hand users must itself be valid");
        cfg.validate(&paths()).unwrap();
    }
}
