//! Parallel filesystem measurement.
//!
//! This is where the win over the original `du -sk` loop lives. Three things
//! matter here and `du` gets all three wrong for our purpose:
//!
//! 1. **`st_blocks`, not `st_size`.** APFS clones and sparse files make apparent
//!    size wildly overstate what you would actually get back.
//! 2. **Hardlink dedup.** pnpm's store and conda's package dir are hardlinked into
//!    every project on the machine. Counting those bytes once per link would have
//!    us promising tens of gigabytes that do not exist.
//! 3. **Device boundaries.** Following a mount into a network share or an external
//!    disk turns a fast scan into a hang.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use dashmap::DashSet;

use crate::model::Size;

/// Shared across an entire scan so that the same inode reached through two
/// different candidates is only counted once, by whichever gets there first.
#[derive(Debug, Default)]
pub struct LinkTracker {
    seen: DashSet<(u64, u64)>,
}

impl LinkTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns true the first time this (device, inode) pair is offered.
    ///
    /// Files with a link count of 1 skip the set entirely: they cannot be shared,
    /// and they are the overwhelming majority, so keeping them out of the shared
    /// structure is what keeps this cheap under contention.
    fn claim(&self, dev: u64, ino: u64, nlink: u64) -> bool {
        if nlink <= 1 {
            return true;
        }
        self.seen.insert((dev, ino))
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Options for a single measurement.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Threads for this walk. 0 inherits the surrounding Rayon pool, 1 walks
    /// serially (required when the caller's own pool has a single thread — see
    /// `measure`), and anything higher spins up a dedicated pool of that size.
    pub threads: usize,
    /// Stay on the device the root lives on.
    pub same_device: bool,
    /// Give up below this many directory levels. Guards against pathological trees.
    pub max_depth: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            same_device: true,
            max_depth: 64,
        }
    }
}

/// Result of measuring one path.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    pub size: Size,
    /// Newest mtime found anywhere in the tree.
    pub newest_mtime: Option<SystemTime>,
    /// Newest atime found anywhere in the tree, when the platform records it.
    pub newest_atime: Option<SystemTime>,
}

/// Measure a directory tree in parallel.
///
/// Never follows symlinks: a link's own (tiny) size is counted, its target is not.
/// Deleting the containing directory only removes the link, so counting the target
/// would overstate the reclaim.
pub fn measure(path: &Path, links: &LinkTracker, opts: &WalkOptions) -> Measurement {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Measurement::default(),
    };

    if !meta.is_dir() {
        return measure_single_file(&meta, links);
    }

    let root_device = device_of(&meta);
    let logical = Arc::new(AtomicU64::new(0));
    let on_disk = Arc::new(AtomicU64::new(0));
    let shared = Arc::new(AtomicU64::new(0));
    let files = Arc::new(AtomicU64::new(0));
    let dirs = Arc::new(AtomicU64::new(1)); // the root itself
    let newest_mtime = Arc::new(AtomicU64::new(0));
    let newest_atime = Arc::new(AtomicU64::new(0));
    let partial = Arc::new(AtomicBool::new(false));

    // jwalk's Rayon-backed modes have to acquire worker threads to make progress.
    // When the caller is already inside a single-threaded Rayon pool — the
    // `--concurrency 1` case — that one thread is blocked right here waiting for
    // the walk, so the nested request can never be served: it burns the whole
    // `busy_timeout` and yields nothing. That surfaced as a scan reporting "0 B"
    // with a bogus "permission errors" note. Walking serially is the right answer
    // there, since a single-threaded caller has no other thread to overlap with.
    let parallelism = match opts.threads {
        0 => jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: std::time::Duration::from_secs(5),
        },
        1 => jwalk::Parallelism::Serial,
        n => jwalk::Parallelism::RayonNewPool(n),
    };

    let walker = jwalk::WalkDirGeneric::<((), ())>::new(path)
        .parallelism(parallelism)
        .skip_hidden(false)
        .follow_links(false)
        .max_depth(opts.max_depth);

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                // Permission denied or a directory removed mid-walk. Record that the
                // total is a floor rather than silently reporting a wrong number.
                partial.store(true, Ordering::Relaxed);
                continue;
            }
        };

        let Ok(meta) = entry.metadata() else {
            partial.store(true, Ordering::Relaxed);
            continue;
        };

        if opts.same_device && device_of(&meta) != root_device {
            partial.store(true, Ordering::Relaxed);
            continue;
        }

        if let Ok(secs) = meta.modified().and_then(|t| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        }) {
            newest_mtime.fetch_max(secs.as_secs(), Ordering::Relaxed);
        }
        if let Ok(secs) = meta.accessed().and_then(|t| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        }) {
            newest_atime.fetch_max(secs.as_secs(), Ordering::Relaxed);
        }

        if meta.is_dir() {
            dirs.fetch_add(1, Ordering::Relaxed);
            on_disk.fetch_add(blocks_of(&meta), Ordering::Relaxed);
            continue;
        }

        files.fetch_add(1, Ordering::Relaxed);
        logical.fetch_add(meta.len(), Ordering::Relaxed);

        let bytes = blocks_of(&meta);
        let (dev, ino, nlink) = identity_of(&meta);
        if links.claim(dev, ino, nlink) {
            on_disk.fetch_add(bytes, Ordering::Relaxed);
        } else {
            shared.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    Measurement {
        size: Size {
            logical: logical.load(Ordering::Relaxed),
            on_disk: on_disk.load(Ordering::Relaxed),
            shared: shared.load(Ordering::Relaxed),
            files: files.load(Ordering::Relaxed),
            dirs: dirs.load(Ordering::Relaxed),
            partial: partial.load(Ordering::Relaxed),
        },
        newest_mtime: to_system_time(newest_mtime.load(Ordering::Relaxed)),
        newest_atime: to_system_time(newest_atime.load(Ordering::Relaxed)),
    }
}

/// A candidate whose path is a single file rather than a tree, e.g. an AVD `.ini`.
fn measure_single_file(meta: &std::fs::Metadata, links: &LinkTracker) -> Measurement {
    let bytes = blocks_of(meta);
    let (dev, ino, nlink) = identity_of(meta);
    let counted = links.claim(dev, ino, nlink);
    Measurement {
        size: Size {
            logical: meta.len(),
            on_disk: if counted { bytes } else { 0 },
            shared: if counted { 0 } else { bytes },
            files: 1,
            dirs: 0,
            partial: false,
        },
        newest_mtime: meta.modified().ok(),
        newest_atime: meta.accessed().ok(),
    }
}

/// Measure several paths that belong to one candidate, sharing the link tracker.
pub fn measure_all(paths: &[PathBuf], links: &LinkTracker, opts: &WalkOptions) -> Measurement {
    paths.iter().fold(Measurement::default(), |acc, p| {
        let m = measure(p, links, opts);
        Measurement {
            size: acc.size + m.size,
            newest_mtime: acc.newest_mtime.max(m.newest_mtime),
            newest_atime: acc.newest_atime.max(m.newest_atime),
        }
    })
}

/// Newest mtime among a project's *source* files, ignoring build artifacts.
///
/// This is what separates a dormant project from an active one. It deliberately
/// refuses to descend into artifact directories: `node_modules` is rewritten by
/// every `npm install` and would make every project look freshly worked on.
pub fn newest_source_mtime(project: &Path, max_depth: usize) -> Option<SystemTime> {
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        "build",
        "dist",
        ".next",
        ".nuxt",
        ".venv",
        "venv",
        "vendor",
        "Pods",
        ".gradle",
        ".idea",
        ".dart_tool",
        "obj",
        "bin",
        "__pycache__",
        ".tox",
        ".mypy_cache",
        ".pytest_cache",
        ".turbo",
        "DerivedData",
    ];

    let newest = AtomicU64::new(0);
    let walker = jwalk::WalkDirGeneric::<((), ())>::new(project)
        .parallelism(jwalk::Parallelism::Serial)
        .skip_hidden(false)
        .follow_links(false)
        .max_depth(max_depth)
        .process_read_dir(|_, _, _, children| {
            children.retain(|child| {
                let Ok(child) = child else { return true };
                let name = child.file_name().to_string_lossy().to_string();
                if child.file_type().is_dir() {
                    // `.git` is skipped here but read separately by `vcs_activity`:
                    // its internal mtimes track fetches, not the user's own work.
                    !SKIP.contains(&name.as_str()) && name != ".git"
                } else {
                    true
                }
            });
        });

    for entry in walker.into_iter().flatten() {
        if entry.file_type().is_dir() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(d) = meta.modified().and_then(|t| {
                t.duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            }) {
                newest.fetch_max(d.as_secs(), Ordering::Relaxed);
            }
        }
    }

    to_system_time(newest.load(Ordering::Relaxed))
}

/// Last git activity in a project, read from the cheap markers rather than by
/// parsing objects: `.git/HEAD` and the ref files move on commit, checkout and fetch.
pub fn vcs_activity(project: &Path) -> Option<SystemTime> {
    let git = project.join(".git");
    if !git.exists() {
        return None;
    }

    // A worktree or submodule has `.git` as a file pointing elsewhere; its own
    // mtime still tracks activity, so it is a usable signal either way.
    let mut newest = std::fs::metadata(&git).ok().and_then(|m| m.modified().ok());

    for rel in ["HEAD", "index", "COMMIT_EDITMSG", "refs/heads", "logs/HEAD"] {
        if let Ok(meta) = std::fs::metadata(git.join(rel)) {
            if let Ok(t) = meta.modified() {
                newest = Some(newest.map_or(t, |n: SystemTime| n.max(t)));
            }
        }
    }

    newest
}

fn to_system_time(secs: u64) -> Option<SystemTime> {
    (secs > 0).then(|| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

#[cfg(unix)]
fn blocks_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // st_blocks is always in 512-byte units by POSIX definition, regardless of
    // the filesystem's own block size.
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn blocks_of(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(unix)]
fn device_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn device_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn identity_of(meta: &std::fs::Metadata) -> (u64, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino(), meta.nlink())
}

#[cfg(not(unix))]
fn identity_of(_meta: &std::fs::Metadata) -> (u64, u64, u64) {
    (0, 0, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn measures_a_simple_tree() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("a.bin"), 4096);
        write(&tmp.path().join("sub/b.bin"), 8192);

        let links = LinkTracker::new();
        let m = measure(tmp.path(), &links, &WalkOptions::default());

        assert_eq!(m.size.files, 2);
        assert_eq!(m.size.logical, 4096 + 8192);
        assert!(
            m.size.on_disk >= 4096 + 8192,
            "on-disk includes directory blocks"
        );
        assert!(!m.size.partial);
        assert!(m.newest_mtime.is_some());
    }

    /// Regression: a single-threaded walk used to report nothing at all.
    ///
    /// `threads: 0` asks jwalk for Rayon-backed parallelism. Called from inside a
    /// single-threaded pool (`--concurrency 1`) the only worker is blocked waiting
    /// on the walk, so the nested request starved, burned the 5s `busy_timeout`
    /// and returned zeroes with `partial` set — a silent wrong answer, reported to
    /// the user as "0 B reclaimable".
    #[test]
    fn a_single_threaded_walk_measures_the_same_bytes_as_a_parallel_one() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("a.bin"), 4096);
        write(&tmp.path().join("sub/b.bin"), 8192);
        write(&tmp.path().join("sub/deeper/c.bin"), 16384);

        let parallel = measure(tmp.path(), &LinkTracker::new(), &WalkOptions::default());
        let serial = measure(
            tmp.path(),
            &LinkTracker::new(),
            &WalkOptions {
                threads: 1,
                ..Default::default()
            },
        );

        assert_eq!(serial.size.logical, 4096 + 8192 + 16384);
        assert_eq!(serial.size.logical, parallel.size.logical);
        assert_eq!(serial.size.files, parallel.size.files);
        assert_eq!(serial.size.on_disk, parallel.size.on_disk);
        assert!(
            !serial.size.partial,
            "a serial walk of a readable tree is complete, not partial"
        );
    }

    #[test]
    fn hardlinked_bytes_are_counted_once_and_reported_as_shared() {
        let tmp = TempDir::new().unwrap();
        let original = tmp.path().join("store/pkg.bin");
        write(&original, 64 * 1024);
        let linked = tmp.path().join("project/pkg.bin");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        fs::hard_link(&original, &linked).unwrap();

        let links = LinkTracker::new();
        let m = measure(tmp.path(), &links, &WalkOptions::default());

        assert_eq!(m.size.files, 2, "both links are visible as files");
        assert_eq!(
            m.size.logical,
            128 * 1024,
            "logical size double-counts, as du does"
        );
        assert!(
            m.size.shared >= 64 * 1024,
            "the second link must land in `shared`"
        );
        // The reclaimable figure must reflect that deleting both only frees 64K once.
        assert!(
            m.size.on_disk < 128 * 1024,
            "on_disk must not double-count hardlinks: {}",
            m.size.on_disk
        );
    }

    #[test]
    fn a_shared_link_tracker_dedups_across_separate_measurements() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store/pkg.bin");
        write(&store, 32 * 1024);
        let proj = tmp.path().join("proj/pkg.bin");
        fs::create_dir_all(proj.parent().unwrap()).unwrap();
        fs::hard_link(&store, &proj).unwrap();

        let links = LinkTracker::new();
        let first = measure(&tmp.path().join("store"), &links, &WalkOptions::default());
        let second = measure(&tmp.path().join("proj"), &links, &WalkOptions::default());

        assert!(first.size.on_disk >= 32 * 1024);
        assert_eq!(
            second.size.shared,
            32 * 1024,
            "second candidate sees the bytes as shared"
        );
        assert!(
            second.size.on_disk < 32 * 1024,
            "and must not claim them again"
        );
    }

    #[test]
    fn symlinks_do_not_pull_in_their_target() {
        let tmp = TempDir::new().unwrap();
        let big = tmp.path().join("outside/big.bin");
        write(&big, 256 * 1024);
        let dir = tmp.path().join("scanned");
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&big, dir.join("link.bin")).unwrap();

        let links = LinkTracker::new();
        let m = measure(&dir, &links, &WalkOptions::default());
        assert!(
            m.size.logical < 256 * 1024,
            "symlink target must not be counted"
        );
    }

    #[test]
    fn a_missing_path_measures_as_zero_rather_than_failing() {
        let links = LinkTracker::new();
        let m = measure(
            Path::new("/definitely/not/here"),
            &links,
            &WalkOptions::default(),
        );
        assert_eq!(m.size.on_disk, 0);
        assert_eq!(m.size.files, 0);
    }

    #[test]
    fn sparse_files_report_their_real_footprint() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sparse.img");
        let f = fs::File::create(&path).unwrap();
        // 64 MB apparent, no blocks allocated.
        f.set_len(64 * 1024 * 1024).unwrap();
        drop(f);

        let links = LinkTracker::new();
        let m = measure(tmp.path(), &links, &WalkOptions::default());
        assert_eq!(m.size.logical, 64 * 1024 * 1024);
        #[cfg(unix)]
        assert!(
            m.size.on_disk < 64 * 1024 * 1024,
            "on-disk must reflect allocated blocks, got {}",
            m.size.on_disk
        );
    }

    #[test]
    fn measure_all_sums_paths_and_keeps_the_newest_mtime() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("one/a.bin"), 1024);
        write(&tmp.path().join("two/b.bin"), 2048);

        let links = LinkTracker::new();
        let m = measure_all(
            &[tmp.path().join("one"), tmp.path().join("two")],
            &links,
            &WalkOptions::default(),
        );
        assert_eq!(m.size.files, 2);
        assert_eq!(m.size.logical, 3072);
    }

    #[test]
    fn source_mtime_ignores_artifact_directories() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path();
        write(&proj.join("src/main.rs"), 10);

        // Make the artifact tree far newer than the source.
        let artifact = proj.join("node_modules/pkg/index.js");
        write(&artifact, 10);
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        let ft = fs::FileTimes::new().set_modified(future);
        fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(ft)
            .unwrap();

        let newest = newest_source_mtime(proj, 8).unwrap();
        assert!(
            newest < future,
            "node_modules must not make a dormant project look active"
        );
    }

    #[test]
    fn vcs_activity_is_none_without_a_repo_and_some_with_one() {
        let tmp = TempDir::new().unwrap();
        assert!(vcs_activity(tmp.path()).is_none());
        fs::create_dir_all(tmp.path().join(".git/refs/heads")).unwrap();
        fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(vcs_activity(tmp.path()).is_some());
    }

    #[test]
    fn link_tracker_only_tracks_files_that_can_actually_be_shared() {
        let t = LinkTracker::default();
        assert!(t.claim(1, 100, 1));
        assert!(
            t.claim(1, 100, 1),
            "single-link files bypass the set entirely"
        );
        assert!(t.is_empty());

        assert!(t.claim(1, 200, 2));
        assert!(
            !t.claim(1, 200, 2),
            "the second sighting of a shared inode is refused"
        );
        assert_eq!(t.len(), 1);
    }
}
