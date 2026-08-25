//! Helpers shared by every provider.
//!
//! Providers are meant to be short and declarative — a list of paths, a tier, and
//! the evidence a user needs to decide. Anything that would otherwise be repeated
//! across sixteen files lives here.

use std::path::{Path, PathBuf};

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Kind, Regen, Tier, Warning};
use reclaim_core::pipeline::ScanContext;
use reclaim_core::ProjectRoot;

/// Declare a global cache directory, if it exists.
///
/// Returns nothing when the path is absent, which is how a provider no-ops
/// cleanly on a machine that does not have that toolchain installed.
pub fn global_cache(
    ctx: &ScanContext,
    provider: &str,
    group: Group,
    label: &str,
    path: PathBuf,
) -> Option<CandidateBuilder> {
    let _ = ctx;
    path.exists().then(|| {
        CandidateBuilder::new(provider, group, label)
            .path(path)
            .kind(Kind::GlobalCache)
            .tier(Tier::Safe)
    })
}

/// A project-local artifact directory, e.g. `target/` or `node_modules`.
///
/// The `project` link is what lets staleness be derived from the owning project's
/// source files rather than from the artifact's own mtime.
pub fn project_artifact(
    provider: &str,
    group: Group,
    label: &str,
    project: &Path,
    artifact: PathBuf,
) -> Option<CandidateBuilder> {
    artifact.is_dir().then(|| {
        CandidateBuilder::new(provider, group, label)
            .path(artifact)
            .kind(Kind::ProjectArtifact)
            .project(project)
            .tier(Tier::Safe)
    })
}

/// Map every project carrying one of `markers` to an artifact directory inside it.
pub fn artifacts_in_projects<F>(ctx: &ScanContext, markers: &[&str], mut build: F) -> Vec<Candidate>
where
    F: FnMut(&ProjectRoot) -> Vec<Candidate>,
{
    ctx.projects_with(markers)
        .iter()
        .flat_map(&mut build)
        .collect()
}

/// Immediate subdirectories of `dir`, sorted for stable output.
pub fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Whether an executable is on `PATH`.
///
/// Used to decide whether a `Shell` candidate is worth offering; there is no
/// point suggesting `brew cleanup` on a machine without Homebrew.
pub fn has_command(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// Whether any of these files exists in `dir`. Used for lockfile detection.
pub fn any_file_exists(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|n| dir.join(n).is_file())
}

/// Count entries in a directory without walking it, for warning text like
/// "contains 14 archives".
pub fn count_entries(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|e| e.flatten().count())
        .unwrap_or(0)
}

/// The standard warning for a content-addressable store that is hardlinked into
/// projects. Deleting it does not corrupt anything, but it does break every
/// existing install until the user reinstalls.
pub fn hardlink_store_warning(tool: &str) -> Warning {
    Warning::caution(format!(
        "This store is hardlinked into every {tool} install on the machine. Removing it \
         frees less than its apparent size and leaves existing installs broken until you \
         reinstall."
    ))
}

/// The standard warning for an artifact directory in a project with no lockfile.
pub fn no_lockfile_warning(tool: &str) -> Warning {
    Warning::caution(format!(
        "No lockfile in this project, so a `{tool}` reinstall may not reproduce these \
         exact versions."
    ))
}

/// Regeneration by re-downloading from a named source.
pub fn redownload(source: &str) -> Regen {
    Regen::Download {
        bytes: None,
        source: source.to_string(),
    }
}

/// Regeneration that happens transparently on the next build.
pub fn automatic(on: &str) -> Regen {
    Regen::Automatic { on: on.to_string() }
}

/// Best-effort: is something listening on `127.0.0.1:port` right now?
///
/// Used to warn when a local AI tool's files may be in active use, not to gate
/// whether a candidate is offered at all — a closed port proves nothing (wrong
/// port, tool not running), but an open one is real evidence.
pub fn port_is_open(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;

    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn subdirs_returns_only_directories_sorted() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("b")).unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("file.txt"), b"x").unwrap();

        let dirs = subdirs(tmp.path());
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].ends_with("a"));
        assert!(dirs[1].ends_with("b"));
    }

    #[test]
    fn subdirs_of_a_missing_directory_is_empty() {
        assert!(subdirs(Path::new("/definitely/not/here")).is_empty());
    }

    #[test]
    fn has_command_finds_a_ubiquitous_binary() {
        assert!(has_command("ls"), "ls must be on PATH");
        assert!(!has_command("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn any_file_exists_detects_lockfiles() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("package-lock.json"), b"{}").unwrap();
        assert!(any_file_exists(
            tmp.path(),
            &["yarn.lock", "package-lock.json"]
        ));
        assert!(!any_file_exists(tmp.path(), &["pnpm-lock.yaml"]));
    }

    #[test]
    fn count_entries_is_zero_for_missing_directories() {
        assert_eq!(count_entries(Path::new("/definitely/not/here")), 0);
    }

    #[test]
    fn project_artifact_is_none_when_the_directory_is_absent() {
        let tmp = TempDir::new().unwrap();
        let built = project_artifact(
            "rust.target",
            Group::Rust,
            "target/",
            tmp.path(),
            tmp.path().join("target"),
        );
        assert!(built.is_none());
    }

    #[test]
    fn port_is_open_detects_a_bound_listener() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_is_open(port));

        drop(listener);
        assert!(!port_is_open(port), "nothing should be listening anymore");
    }

    #[test]
    fn project_artifact_links_back_to_its_project() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();

        let candidate = project_artifact("rust.target", Group::Rust, "target/", tmp.path(), target)
            .unwrap()
            .build();

        assert_eq!(candidate.kind, Kind::ProjectArtifact);
        assert_eq!(candidate.project.as_deref(), Some(tmp.path()));
    }
}
