//! The guard every destructive operation must clear.
//!
//! Design rule: this module fails closed. Anything it cannot positively prove is
//! safe is refused. There is no bypass flag, no `--force` that skips it, and no
//! code path in the executor that deletes without calling [`PathGuard::check`].
//!
//! The check runs twice: once at scan time, and again immediately before deletion.
//! A user may sit in the TUI or the web UI for several minutes between those two
//! moments, and a symlink can be swapped underneath us in that window.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::platform::Paths;

/// Absolute paths that are never deletable regardless of configuration.
///
/// These are the "you would ruin someone's day" list. `protected_paths` in the
/// config is additive to this; nothing can subtract from it.
const ALWAYS_PROTECTED: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/opt",
    "/private",
    "/proc",
    "/root",
    "/sbin",
    "/srv",
    "/sys",
    "/usr",
    "/var",
    "/Applications",
    "/Library",
    "/System",
    "/Users",
    "/Volumes",
];

/// Names that are never deletable relative to the home directory.
const PROTECTED_HOME_ENTRIES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".config",
    ".password-store",
    "Documents",
    "Desktop",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Public",
    "Applications",
];

/// Path components that must never appear in a deletion target, at any depth.
///
/// `.git` is the important one: losing it destroys history that may exist nowhere
/// else, and no cache is ever legitimately stored inside it.
const FORBIDDEN_COMPONENTS: &[&str] = &[".git", ".hg", ".svn", ".Trash", ".Trashes"];

/// Validates paths before any destructive action.
#[derive(Debug, Clone)]
pub struct PathGuard {
    home: PathBuf,
    /// Deletion targets must live under one of these.
    allowed_roots: Vec<PathBuf>,
    /// User-configured additions to the protected set.
    protected: BTreeSet<PathBuf>,
    /// Minimum number of path components below the home directory.
    /// Depth 1 (`~/foo`) is allowed for well-known dotted caches like `~/.npm`;
    /// see [`PathGuard::check`] for why bare non-dotted entries still need depth 2.
    min_depth_below_home: usize,
}

impl PathGuard {
    /// Build a guard rooted at the user's home directory.
    pub fn new(paths: &Paths) -> Self {
        let home = paths.home().to_path_buf();
        let mut allowed_roots = vec![home.clone()];

        // Caches and SDKs legitimately live outside `$HOME` when the user has
        // relocated them via environment variables.
        for external in [
            paths.cargo_home(),
            paths.gopath(),
            paths.go_build_cache(),
            paths.android_avd(),
        ] {
            if !external.starts_with(&home) {
                allowed_roots.push(external);
            }
        }
        if let Some(sdk) = paths.android_sdk() {
            if !sdk.starts_with(&home) {
                allowed_roots.push(sdk);
            }
        }
        if let Some(tmp) = paths.tmpdir() {
            allowed_roots.push(tmp.to_path_buf());
        }

        let protected = PROTECTED_HOME_ENTRIES
            .iter()
            .map(|e| home.join(e))
            .collect();

        Self {
            home,
            allowed_roots,
            protected,
            min_depth_below_home: 1,
        }
    }

    /// Add user-configured protected paths. Additive only.
    pub fn protect(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.protected.extend(paths);
        self
    }

    /// Permit deletion under an additional root, e.g. a configured project root
    /// that lives outside `$HOME`.
    pub fn allow_root(mut self, root: PathBuf) -> Self {
        self.allowed_roots.push(root);
        self
    }

    /// The core check. Returns the canonical path on success.
    ///
    /// Canonicalisation happens first and everything downstream reasons about the
    /// resolved path, so `~/.npm/../../../etc` cannot slip through by looking
    /// well-formed as a string.
    pub fn check(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute() {
            return Err(Error::refused(path, "path is not absolute"));
        }

        // A symlink target could be anywhere; deleting through one is never what
        // the user meant. Reject the link itself rather than following it.
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| Error::refused(path, format!("cannot stat: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(Error::refused(path, "path is a symlink"));
        }

        let canonical = path
            .canonicalize()
            .map_err(|e| Error::refused(path, format!("cannot canonicalize: {e}")))?;

        // Canonicalisation can move the path somewhere else entirely if any parent
        // component was a symlink, so every subsequent check uses `canonical`.
        self.check_canonical(&canonical)?;
        Ok(canonical)
    }

    /// The path-shape rules, split out so they are testable without touching disk.
    pub fn check_canonical(&self, canonical: &Path) -> Result<()> {
        if canonical.components().count() <= 1 {
            return Err(Error::refused(
                canonical,
                "refusing to operate on the filesystem root",
            ));
        }

        for protected in ALWAYS_PROTECTED {
            let protected = Path::new(protected);
            if canonical == protected {
                return Err(Error::refused(canonical, "system directory"));
            }
        }

        if canonical == self.home {
            return Err(Error::refused(canonical, "this is your home directory"));
        }

        for protected in &self.protected {
            if canonical == protected || canonical.starts_with(protected) {
                return Err(Error::refused(
                    canonical,
                    format!("inside protected path {}", protected.display()),
                ));
            }
        }

        for component in canonical.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy();
                if FORBIDDEN_COMPONENTS.iter().any(|f| *f == name) {
                    return Err(Error::refused(
                        canonical,
                        format!("path contains a `{name}` component"),
                    ));
                }
            }
        }

        let under_allowed_root = self
            .allowed_roots
            .iter()
            .any(|root| canonical.starts_with(root));
        if !under_allowed_root {
            return Err(Error::refused(
                canonical,
                "outside every allowed root (home, cargo/go/android dirs, TMPDIR)",
            ));
        }

        if let Ok(relative) = canonical.strip_prefix(&self.home) {
            let depth = relative.components().count();
            if depth < self.min_depth_below_home {
                return Err(Error::refused(canonical, "too close to the home directory"));
            }
            // A single non-hidden entry directly in `$HOME` is almost certainly a
            // real folder of the user's ("~/Projects"), not a tool cache. Hidden
            // entries at depth 1 are the normal shape for caches (`~/.npm`,
            // `~/.gradle`) and are allowed.
            if depth == 1 {
                let name = relative.to_string_lossy();
                if !name.starts_with('.') {
                    return Err(Error::refused(
                        canonical,
                        "top-level non-hidden directory in your home folder",
                    ));
                }
            }
        }

        Ok(())
    }

    /// Check a batch, returning the canonical paths that passed and the refusals.
    ///
    /// Missing paths are silently dropped rather than reported as failures: for a
    /// cleanup tool, "already gone" is success.
    pub fn check_all(&self, paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<Error>) {
        let mut ok = Vec::new();
        let mut errs = Vec::new();
        for path in paths {
            if !path.exists() {
                continue;
            }
            match self.check(path) {
                Ok(canonical) => ok.push(canonical),
                Err(e) => errs.push(e),
            }
        }
        (ok, errs)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A guard over a real temporary home directory, so `canonicalize` works.
    fn guard() -> (TempDir, PathGuard) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let paths = Paths::with_home(&home);
        let guard = PathGuard::new(&paths);
        (tmp, guard)
    }

    fn mkdir(base: &Path, rel: &str) -> PathBuf {
        let p = base.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn accepts_a_normal_hidden_cache_directory() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let npm = mkdir(&home, ".npm/_cacache");
        assert!(g.check(&npm).is_ok());
        // Depth 1 hidden is the shape of `~/.npm` itself, and must also pass.
        assert!(g.check(&home.join(".npm")).is_ok());
    }

    #[test]
    fn refuses_the_home_directory_itself() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let err = g.check(&home).unwrap_err().to_string();
        assert!(err.contains("home directory"), "{err}");
    }

    #[test]
    fn refuses_the_filesystem_root() {
        let (_tmp, g) = guard();
        assert!(g.check(Path::new("/")).is_err());
    }

    #[test]
    fn refuses_system_directories() {
        let (_tmp, g) = guard();
        for path in ["/etc", "/usr", "/System", "/Applications"] {
            let p = Path::new(path);
            if p.exists() {
                assert!(g.check(p).is_err(), "{path} must be refused");
            }
        }
    }

    #[test]
    fn refuses_relative_paths() {
        let (_tmp, g) = guard();
        let err = g.check(Path::new("relative/path")).unwrap_err().to_string();
        assert!(err.contains("not absolute"), "{err}");
    }

    #[test]
    fn refuses_protected_home_entries_and_their_children() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let ssh = mkdir(&home, ".ssh");
        let inner = mkdir(&home, ".ssh/keys");
        assert!(g.check(&ssh).is_err());
        assert!(
            g.check(&inner).is_err(),
            "children of protected paths must be refused too"
        );
        let docs = mkdir(&home, "Documents/work");
        assert!(g.check(&docs).is_err());
    }

    #[test]
    fn refuses_user_configured_protected_paths() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let precious = mkdir(&home, ".cache/precious");
        let g = g.protect([precious.clone()]);
        assert!(g.check(&precious).is_err());
        assert!(g.check(&mkdir(&home, ".cache/precious/deep")).is_err());
    }

    #[test]
    fn refuses_anything_containing_a_dot_git_component() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let git = mkdir(&home, "dev/proj/.git");
        let inside = mkdir(&home, "dev/proj/.git/objects");
        assert!(
            g.check(&git).is_err(),
            "deleting .git destroys unrecoverable history"
        );
        assert!(g.check(&inside).is_err());
    }

    #[test]
    fn refuses_a_symlink_rather_than_following_it() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let real = mkdir(&home, ".cache/real");
        let link = home.join(".cache/link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(unix)]
        {
            let err = g.check(&link).unwrap_err().to_string();
            assert!(err.contains("symlink"), "{err}");
        }
        let _ = real;
    }

    #[test]
    fn refuses_traversal_that_escapes_the_allowed_roots() {
        // The string looks like it is inside home, but resolves to /etc.
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        mkdir(&home, ".cache");
        let escape = home.join(".cache/../../../../../../etc");
        if escape.exists() {
            let err = g.check(&escape).unwrap_err().to_string();
            assert!(
                err.contains("system directory") || err.contains("outside every allowed root"),
                "traversal must be caught after canonicalization: {err}"
            );
        }
    }

    #[test]
    fn refuses_paths_outside_every_allowed_root() {
        let (_tmp, g) = guard();
        let other = TempDir::new().unwrap();
        let stray = other.path().canonicalize().unwrap().join("stuff");
        fs::create_dir_all(&stray).unwrap();
        // TMPDIR may legitimately cover this on macOS; only assert when it does not.
        let tmpdir = std::env::var_os("TMPDIR").map(PathBuf::from);
        let covered = tmpdir.is_some_and(|t| {
            t.canonicalize()
                .map(|t| stray.starts_with(t))
                .unwrap_or(false)
        });
        if !covered {
            let err = g.check(&stray).unwrap_err().to_string();
            assert!(err.contains("outside every allowed root"), "{err}");
        }
    }

    #[test]
    fn refuses_top_level_non_hidden_home_folders() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let projects = mkdir(&home, "Projects");
        let err = g.check(&projects).unwrap_err().to_string();
        assert!(err.contains("top-level non-hidden"), "{err}");
        // ...but a build artifact one level deeper is fine.
        assert!(g.check(&mkdir(&home, "Projects/app/node_modules")).is_ok());
    }

    #[test]
    fn check_all_skips_missing_and_collects_refusals() {
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let good = mkdir(&home, ".npm/_cacache");
        let bad = mkdir(&home, ".ssh");
        let missing = home.join(".nope");
        let (ok, errs) = g.check_all(&[good.clone(), bad, missing]);
        assert_eq!(ok, vec![good]);
        assert_eq!(errs.len(), 1, "missing paths are not errors, refusals are");
    }

    #[test]
    fn a_swapped_symlink_is_caught_on_recheck() {
        // Simulates the TOCTOU window: a real directory passes at scan time, is
        // replaced by a symlink while the user deliberates, and must fail at delete time.
        let (tmp, g) = guard();
        let home = tmp.path().canonicalize().unwrap();
        let target = mkdir(&home, ".cache/target");
        assert!(g.check(&target).is_ok(), "passes at scan time");

        #[cfg(unix)]
        {
            fs::remove_dir(&target).unwrap();
            std::os::unix::fs::symlink(home.join(".ssh"), &target).unwrap();
            assert!(
                g.check(&target).is_err(),
                "must fail on re-check before deletion"
            );
        }
    }
}
