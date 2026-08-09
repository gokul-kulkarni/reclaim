//! The domain model. Every other module is a transform over these values.
//!
//! Candidates are immutable: the pipeline stages (`discover` -> `measure` -> `score`)
//! each consume a candidate and return a new one rather than mutating in place.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Stable identifier for a candidate, derived from provider id + primary path.
/// Stable across runs so the web UI can reference an item after a rescan.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CandidateId(pub String);

impl CandidateId {
    pub fn new(provider: &str, primary_path: &std::path::Path) -> Self {
        // FNV-1a over the path bytes; short, stable, and dependency-free.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in primary_path.as_os_str().as_encoded_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(format!("{provider}:{hash:016x}"))
    }
}

impl fmt::Display for CandidateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Ecosystem grouping, used for the TUI tree and the web UI treemap's first level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Group {
    Node,
    Python,
    Rust,
    Jvm,
    Go,
    Apple,
    Android,
    Containers,
    DotNet,
    Ruby,
    Php,
    Dart,
    BuildTools,
    Editors,
    System,
    Ml,
}

impl Group {
    pub const ALL: &'static [Group] = &[
        Group::Node,
        Group::Python,
        Group::Rust,
        Group::Jvm,
        Group::Go,
        Group::Apple,
        Group::Android,
        Group::Containers,
        Group::DotNet,
        Group::Ruby,
        Group::Php,
        Group::Dart,
        Group::BuildTools,
        Group::Editors,
        Group::System,
        Group::Ml,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Group::Node => "node",
            Group::Python => "python",
            Group::Rust => "rust",
            Group::Jvm => "jvm",
            Group::Go => "go",
            Group::Apple => "apple",
            Group::Android => "android",
            Group::Containers => "containers",
            Group::DotNet => "dotnet",
            Group::Ruby => "ruby",
            Group::Php => "php",
            Group::Dart => "dart",
            Group::BuildTools => "buildtools",
            Group::Editors => "editors",
            Group::System => "system",
            Group::Ml => "ml",
        }
    }

    /// Human-facing label for headings.
    pub fn title(self) -> &'static str {
        match self {
            Group::Node => "Node.js",
            Group::Python => "Python",
            Group::Rust => "Rust",
            Group::Jvm => "JVM / Java",
            Group::Go => "Go",
            Group::Apple => "Apple / Xcode",
            Group::Android => "Android",
            Group::Containers => "Containers",
            Group::DotNet => ".NET",
            Group::Ruby => "Ruby",
            Group::Php => "PHP",
            Group::Dart => "Dart / Flutter",
            Group::BuildTools => "Build tools",
            Group::Editors => "Editors & IDEs",
            Group::System => "System",
            Group::Ml => "ML / models",
        }
    }
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Group {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Group::ALL
            .iter()
            .copied()
            .find(|g| g.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown group `{s}`"))
    }
}

/// How dangerous it is to remove this candidate.
///
/// This replaces the original shell script's binary safe/risky split, because
/// "re-downloadable at a cost" and "irreplaceable" deserve different treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// Regenerates automatically on the next build. No user action needed.
    Safe,
    /// Recoverable, but costs a re-download or a long rebuild.
    Review,
    /// May be irreplaceable. Never removed without explicit confirmation.
    Caution,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Safe => "safe",
            Tier::Review => "review",
            Tier::Caution => "caution",
        }
    }

    /// Weight in the reclaim score: riskier items are ranked lower.
    pub fn weight(self) -> f64 {
        match self {
            Tier::Safe => 1.0,
            Tier::Review => 0.6,
            Tier::Caution => 0.25,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "safe" => Ok(Tier::Safe),
            "review" => Ok(Tier::Review),
            "caution" => Ok(Tier::Caution),
            other => Err(format!(
                "unknown tier `{other}` (expected safe|review|caution)"
            )),
        }
    }
}

/// What kind of thing this is, which drives how staleness is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A shared cache in the home directory, e.g. `~/.npm`.
    GlobalCache,
    /// An artifact belonging to one project, e.g. `target/` or `node_modules`.
    /// Staleness for these comes from the *owning project*, not the artifact.
    ProjectArtifact,
    /// A downloadable runtime or SDK image, e.g. a simulator runtime.
    Runtime,
    /// Reclaimed by shelling out to a tool rather than deleting paths.
    Command,
}

/// How the space comes back if the user later needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Regen {
    /// Rebuilt transparently by the toolchain. No user-visible cost.
    Automatic { on: String },
    /// Re-downloaded from a network source.
    Download { bytes: Option<u64>, source: String },
    /// Rebuilt locally, costing CPU time.
    Rebuild { minutes: u32 },
    /// Gone forever.
    Never,
}

impl Regen {
    /// Weight in the reclaim score: expensive-to-restore items are ranked lower.
    pub fn weight(&self) -> f64 {
        match self {
            Regen::Automatic { .. } => 1.0,
            Regen::Download { .. } => 1.4,
            Regen::Rebuild { .. } => 1.8,
            Regen::Never => 4.0,
        }
    }

    /// Short human phrase for the "comes back" column.
    pub fn summary(&self) -> String {
        match self {
            Regen::Automatic { on } => format!("auto on {on}"),
            Regen::Download {
                bytes: Some(b),
                source,
            } => {
                format!("{} from {source}", crate::format::bytes(*b))
            }
            Regen::Download {
                bytes: None,
                source,
            } => format!("re-download from {source}"),
            Regen::Rebuild { minutes } => format!("~{minutes} min rebuild"),
            Regen::Never => "gone forever".to_string(),
        }
    }
}

/// A piece of evidence to show the user before they decide.
///
/// This is the heart of the tool's value over `du -sh`: not just how big something
/// is, but what it will cost you to be wrong about deleting it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub severity: Severity,
    pub message: String,
}

impl Warning {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            message: message.into(),
        }
    }

    pub fn caution(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Caution,
            message: message.into(),
        }
    }

    pub fn danger(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Danger,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Caution,
    Danger,
}

/// How the reclaim is actually performed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Remove `paths` from disk (permanently or to Trash, per the delete mode).
    RemovePaths,
    /// Run an external command that does the reclaiming itself.
    Shell { program: String, args: Vec<String> },
}

impl Action {
    pub fn is_shell(&self) -> bool {
        matches!(self, Action::Shell { .. })
    }
}

/// Measured size of a candidate.
///
/// `on_disk` is the number that matters: it is computed from `st_blocks`, so it
/// accounts for sparse files and APFS clones, unlike `st_size`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Sum of apparent file sizes (`st_size`). What `du --apparent-size` reports.
    pub logical: u64,
    /// Actual blocks occupied, excluding bytes already counted via another hardlink.
    /// This is the space you get back.
    pub on_disk: u64,
    /// Bytes skipped because another hardlink to the same inode was already counted.
    /// Large values mean the store is shared (pnpm, conda) and deleting it affects others.
    pub shared: u64,
    pub files: u64,
    pub dirs: u64,
    /// True if the walk was cut short (permission errors, depth limit).
    pub partial: bool,
}

impl Size {
    /// Ratio of this candidate's bytes that are hardlinked elsewhere, 0.0..=1.0.
    pub fn shared_ratio(&self) -> f64 {
        let total = self.on_disk + self.shared;
        if total == 0 {
            0.0
        } else {
            self.shared as f64 / total as f64
        }
    }
}

impl std::ops::Add for Size {
    type Output = Size;

    fn add(self, rhs: Size) -> Size {
        Size {
            logical: self.logical + rhs.logical,
            on_disk: self.on_disk + rhs.on_disk,
            shared: self.shared + rhs.shared,
            files: self.files + rhs.files,
            dirs: self.dirs + rhs.dirs,
            partial: self.partial || rhs.partial,
        }
    }
}

impl std::iter::Sum for Size {
    fn sum<I: Iterator<Item = Size>>(iter: I) -> Size {
        iter.fold(Size::default(), |acc, s| acc + s)
    }
}

/// Freshness evidence gathered during measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    /// Newest mtime anywhere in the artifact tree.
    pub artifact_mtime: SystemTime,
    /// Newest atime, when the filesystem records it usefully.
    pub artifact_atime: Option<SystemTime>,
    /// Newest mtime among the owning project's *source* files (not its artifacts).
    pub source_mtime: Option<SystemTime>,
    /// Last observed git activity in the owning project.
    pub vcs_activity: Option<SystemTime>,
    /// Days since the most recent of all the above signals.
    pub last_used_days: u32,
    /// Touched within the last 24h: a build or dev server may be using it right now.
    pub active_now: bool,
}

impl Signals {
    /// The single moment this candidate was last meaningfully used.
    ///
    /// For a project artifact this deliberately prefers the *project's* activity over
    /// the artifact's own mtime: a `target/` directory untouched for six months still
    /// belongs to a repository you committed to yesterday, and is not stale.
    pub fn last_used(&self) -> SystemTime {
        [
            Some(self.artifact_mtime),
            self.artifact_atime,
            self.source_mtime,
            self.vcs_activity,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(self.artifact_mtime)
    }
}

/// One reclaimable thing, as offered to the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: CandidateId,
    /// Dotted provider id, e.g. `node.pnpm-store`.
    pub provider: String,
    pub group: Group,
    pub label: String,
    /// Longer explanation shown in the detail pane.
    pub detail: String,
    pub paths: Vec<PathBuf>,
    pub kind: Kind,
    pub tier: Tier,
    pub action: Action,
    pub regen: Regen,
    pub warnings: Vec<Warning>,
    /// The project this artifact belongs to, for `Kind::ProjectArtifact`.
    pub project: Option<PathBuf>,
    /// Filled by the measure stage.
    pub size: Option<Size>,
    /// Filled by the measure stage.
    pub signals: Option<Signals>,
    /// Filled by the score stage, 0.0..=100.0.
    pub score: Option<f64>,
}

impl Candidate {
    /// Bytes this candidate would actually free. Zero until measured.
    pub fn reclaimable(&self) -> u64 {
        self.size.map_or(0, |s| s.on_disk)
    }

    pub fn last_used_days(&self) -> Option<u32> {
        self.signals.as_ref().map(|s| s.last_used_days)
    }

    pub fn primary_path(&self) -> Option<&std::path::Path> {
        self.paths.first().map(|p| p.as_path())
    }

    pub fn max_severity(&self) -> Option<Severity> {
        self.warnings.iter().map(|w| w.severity).max()
    }

    /// Immutable update: returns a copy carrying measurement results.
    pub fn with_measurement(&self, size: Size, signals: Signals) -> Candidate {
        Candidate {
            size: Some(size),
            signals: Some(signals),
            ..self.clone()
        }
    }

    /// Immutable update: returns a copy carrying a score.
    pub fn with_score(&self, score: f64) -> Candidate {
        Candidate {
            score: Some(score),
            ..self.clone()
        }
    }
}

/// A builder so providers can declare candidates readably without a 14-field literal.
#[derive(Debug, Clone)]
pub struct CandidateBuilder {
    provider: String,
    group: Group,
    label: String,
    detail: String,
    paths: Vec<PathBuf>,
    kind: Kind,
    tier: Tier,
    action: Action,
    regen: Regen,
    warnings: Vec<Warning>,
    project: Option<PathBuf>,
}

impl CandidateBuilder {
    pub fn new(provider: impl Into<String>, group: Group, label: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            group,
            label: label.into(),
            detail: String::new(),
            paths: Vec::new(),
            kind: Kind::GlobalCache,
            tier: Tier::Safe,
            action: Action::RemovePaths,
            regen: Regen::Automatic {
                on: "next build".into(),
            },
            warnings: Vec::new(),
            project: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }

    pub fn paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.paths.extend(paths);
        self
    }

    pub fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    pub fn tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.action = action;
        self
    }

    pub fn regen(mut self, regen: Regen) -> Self {
        self.regen = regen;
        self
    }

    pub fn warn(mut self, warning: Warning) -> Self {
        self.warnings.push(warning);
        self
    }

    pub fn warn_if(self, condition: bool, warning: Warning) -> Self {
        if condition {
            self.warn(warning)
        } else {
            self
        }
    }

    pub fn project(mut self, project: impl Into<PathBuf>) -> Self {
        self.project = Some(project.into());
        self
    }

    pub fn build(self) -> Candidate {
        let primary = self
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&self.provider));
        Candidate {
            id: CandidateId::new(&self.provider, &primary),
            provider: self.provider,
            group: self.group,
            label: self.label,
            detail: self.detail,
            paths: self.paths,
            kind: self.kind,
            tier: self.tier,
            action: self.action,
            regen: self.regen,
            warnings: self.warnings,
            project: self.project,
            size: None,
            signals: None,
            score: None,
        }
    }
}

/// A project discovered by the single stage-1 walk, shared by every provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoot {
    pub path: PathBuf,
    /// Marker filenames found in this directory, e.g. `package.json`, `Cargo.toml`.
    pub markers: Vec<String>,
}

impl ProjectRoot {
    pub fn has_marker(&self, name: &str) -> bool {
        self.markers.iter().any(|m| m == name)
    }

    pub fn has_any_marker(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.has_marker(n))
    }
}

/// How long ago, as a coarse human phrase ("4 months ago").
pub fn humanize_age(days: u32) -> String {
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=13 => format!("{days} days ago"),
        14..=59 => format!("{} weeks ago", days / 7),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

/// Days between `then` and now, saturating at 0 for future timestamps.
pub fn days_since(then: SystemTime) -> u32 {
    SystemTime::now()
        .duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .saturating_div(86_400)
        .min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_id_is_stable_for_same_path() {
        let a = CandidateId::new("node.npm", std::path::Path::new("/home/x/.npm"));
        let b = CandidateId::new("node.npm", std::path::Path::new("/home/x/.npm"));
        assert_eq!(a, b);
    }

    #[test]
    fn candidate_id_differs_across_paths_and_providers() {
        let a = CandidateId::new("node.npm", std::path::Path::new("/home/x/.npm"));
        let b = CandidateId::new("node.npm", std::path::Path::new("/home/y/.npm"));
        let c = CandidateId::new("node.yarn", std::path::Path::new("/home/x/.npm"));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn sizes_add_and_propagate_partial() {
        let a = Size {
            on_disk: 10,
            files: 1,
            partial: false,
            ..Size::default()
        };
        let b = Size {
            on_disk: 5,
            files: 2,
            partial: true,
            ..Size::default()
        };
        let sum = a + b;
        assert_eq!(sum.on_disk, 15);
        assert_eq!(sum.files, 3);
        assert!(
            sum.partial,
            "partial must be sticky so we never claim a total is exact"
        );
    }

    #[test]
    fn shared_ratio_reports_hardlinked_fraction() {
        let s = Size {
            on_disk: 25,
            shared: 75,
            ..Size::default()
        };
        assert!((s.shared_ratio() - 0.75).abs() < f64::EPSILON);
        assert_eq!(Size::default().shared_ratio(), 0.0);
    }

    #[test]
    fn last_used_prefers_project_activity_over_stale_artifact() {
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let recent = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000_000);
        let signals = Signals {
            artifact_mtime: old,
            artifact_atime: None,
            source_mtime: Some(recent),
            vcs_activity: None,
            last_used_days: 0,
            active_now: false,
        };
        assert_eq!(signals.last_used(), recent);
    }

    #[test]
    fn tier_parses_case_insensitively_and_rejects_junk() {
        assert_eq!("SAFE".parse::<Tier>().unwrap(), Tier::Safe);
        assert!("nonsense".parse::<Tier>().is_err());
    }

    #[test]
    fn humanize_age_reads_naturally() {
        assert_eq!(humanize_age(0), "today");
        assert_eq!(humanize_age(1), "yesterday");
        assert_eq!(humanize_age(5), "5 days ago");
        assert_eq!(humanize_age(21), "3 weeks ago");
        assert_eq!(humanize_age(120), "4 months ago");
        assert_eq!(humanize_age(800), "2 years ago");
    }

    #[test]
    fn builder_defaults_to_the_conservative_option() {
        let c = CandidateBuilder::new("node.npm", Group::Node, "npm cache")
            .path("/home/x/.npm")
            .build();
        assert_eq!(c.tier, Tier::Safe);
        assert_eq!(c.action, Action::RemovePaths);
        assert_eq!(
            c.reclaimable(),
            0,
            "unmeasured candidates must not claim any space"
        );
    }
}
