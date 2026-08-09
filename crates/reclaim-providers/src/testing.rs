//! A synthetic home directory for provider tests.
//!
//! Every provider is tested against a real filesystem tree rather than a mock, so
//! the tests exercise the same `exists()` / `read_dir` calls that run in
//! production. `TestHome` makes building one a few lines instead of twenty.
//!
//! Compiled into the library (not `#[cfg(test)]`-only) so the integration tests in
//! `tests/` can use it too.

use std::path::{Path, PathBuf};

use reclaim_core::config::Config;
use reclaim_core::model::{Candidate, ProjectRoot};
use reclaim_core::pipeline::{Provider, ScanContext};
use reclaim_core::platform::Base;
use reclaim_core::Paths;

/// A temporary home directory that is cleaned up when dropped.
pub struct TestHome {
    tmp: tempfile::TempDir,
    home: PathBuf,
    pub config: Config,
    projects: std::cell::RefCell<Vec<ProjectRoot>>,
    /// Base-directory redirects, applied to the `Paths` handed to providers.
    /// Set through these rather than through `$CARGO_HOME` and friends: the
    /// environment is process-global and tests run in parallel.
    overrides: std::cell::RefCell<Vec<(Base, PathBuf)>>,
}

impl Default for TestHome {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHome {
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("create temp home");
        // Canonicalise so that comparisons against paths the guard has resolved
        // line up on macOS, where /var is a symlink to /private/var.
        let home = tmp.path().canonicalize().expect("canonicalize temp home");
        Self {
            tmp,
            home,
            config: Config::default(),
            projects: std::cell::RefCell::new(Vec::new()),
            overrides: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Absolute path for a home-relative fragment.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    pub fn paths(&self) -> Paths {
        self.overrides
            .borrow()
            .iter()
            .fold(Paths::with_home(&self.home), |paths, (base, path)| {
                paths.with_override(*base, path)
            })
    }

    /// Redirect a toolchain base directory into the sandbox, e.g. point
    /// `CARGO_HOME` at `<home>/sandbox-cargo` without setting any env var.
    pub fn redirect(&self, base: Base, rel: &str) -> &Self {
        self.overrides.borrow_mut().push((base, self.path(rel)));
        self
    }

    /// Create a directory, returning self so calls chain.
    pub fn dir(&self, rel: &str) -> &Self {
        std::fs::create_dir_all(self.path(rel)).expect("create dir");
        self
    }

    /// Create a file of `bytes` length, creating parent directories as needed.
    pub fn file(&self, rel: &str, bytes: usize) -> &Self {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, vec![b'x'; bytes]).expect("write file");
        self
    }

    /// Create a project directory with the given marker files, and register it so
    /// providers see it exactly as the stage-1 walk would have produced it.
    pub fn project(&self, rel: &str, markers: &[&str]) -> &Self {
        self.dir(rel);
        for marker in markers {
            self.file(&format!("{rel}/{marker}"), 32);
        }
        self.projects.borrow_mut().push(ProjectRoot {
            path: self.path(rel),
            markers: markers.iter().map(|m| m.to_string()).collect(),
        });
        self
    }

    /// Create a hardlink, for exercising shared-store accounting.
    pub fn hardlink(&self, from: &str, to: &str) -> &Self {
        let to_path = self.path(to);
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::hard_link(self.path(from), to_path).expect("hard link");
        self
    }

    /// Set a file or directory's mtime, for staleness tests.
    pub fn set_mtime(&self, rel: &str, ago: std::time::Duration) -> &Self {
        let when = std::time::SystemTime::now() - ago;
        let times = std::fs::FileTimes::new()
            .set_modified(when)
            .set_accessed(when);
        let path = self.path(rel);
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open for times");
        file.set_times(times).expect("set times");
        self
    }

    /// The context a provider is handed during a scan.
    pub fn context(&self) -> ScanContext {
        ScanContext::new(
            self.paths(),
            self.config.clone(),
            self.projects.borrow().clone(),
        )
    }

    /// Run one provider against this home and return what it offers.
    pub fn discover(&self, provider: &dyn Provider) -> Vec<Candidate> {
        provider.discover(&self.context())
    }

    /// Run several providers, as a scan would.
    pub fn discover_all(&self, providers: &[Box<dyn Provider>]) -> Vec<Candidate> {
        providers
            .iter()
            .filter(|p| self.config.providers.is_enabled(p.id()))
            .flat_map(|p| p.discover(&self.context()))
            .collect()
    }

    /// Keep the directory on disk after the test, for debugging.
    pub fn leak(self) -> PathBuf {
        let path = self.tmp.keep();
        eprintln!("TestHome left at {}", path.display());
        path
    }
}

/// Assert that a provider offered a candidate with this id, returning it.
pub fn expect_candidate<'a>(candidates: &'a [Candidate], provider: &str) -> &'a Candidate {
    candidates
        .iter()
        .find(|c| c.provider == provider)
        .unwrap_or_else(|| {
            panic!(
                "no candidate from `{provider}`; got: {:?}",
                candidates.iter().map(|c| &c.provider).collect::<Vec<_>>()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_tree_and_reports_absolute_paths() {
        let home = TestHome::new();
        home.dir(".npm/_cacache").file(".npm/_cacache/index", 128);
        assert!(home.path(".npm/_cacache/index").is_file());
        assert!(home.home().is_absolute());
    }

    #[test]
    fn registered_projects_appear_in_the_scan_context() {
        let home = TestHome::new();
        home.project("dev/app", &["package.json", "package-lock.json"]);

        let ctx = home.context();
        let node_projects = ctx.projects_with(&["package.json"]);
        assert_eq!(node_projects.len(), 1);
        assert!(node_projects[0].has_marker("package-lock.json"));
    }

    #[test]
    fn hardlinks_share_an_inode() {
        let home = TestHome::new();
        home.file("store/blob", 1024)
            .hardlink("store/blob", "project/blob");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(home.path("store/blob")).unwrap();
            let b = std::fs::metadata(home.path("project/blob")).unwrap();
            assert_eq!(a.ino(), b.ino());
        }
    }

    #[test]
    fn mtimes_can_be_pushed_into_the_past() {
        let home = TestHome::new();
        home.file("old.bin", 16)
            .set_mtime("old.bin", std::time::Duration::from_secs(86_400 * 90));
        let meta = std::fs::metadata(home.path("old.bin")).unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(meta.modified().unwrap())
            .unwrap();
        assert!(age.as_secs() > 86_400 * 80, "mtime should be ~90 days old");
    }
}
