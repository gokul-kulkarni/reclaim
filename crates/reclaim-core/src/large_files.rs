//! Find individual large files, wherever they are.
//!
//! The rest of the engine knows *what* a directory is (a cache, a build
//! artifact) and therefore what it costs to lose. A large file found here is
//! different: it is almost always the user's own data — a screen recording, a
//! VM image, a forgotten `.dmg` — and nothing can say whether it is replaceable.
//! So this module only ever *reports*. Removal goes through
//! [`crate::exec::trash_files`], which is Trash-only and re-checks every path.
//!
//! Directories that the provider pipeline already understands (`node_modules`,
//! `target`, `.venv`, …) are pruned: their contents are offered, with proper
//! context, by `reclaim scan`, and a single file inside them is never a sensible
//! thing to delete on its own.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::discovery::{compile_excludes, is_excluded, NEVER_DESCEND};
use crate::error::Result;
use crate::model::{Action, Candidate, CandidateBuilder, Group, Kind, Regen, Tier, Warning};
use crate::walk::{blocks_of, device_of, identity_of};

/// Default threshold for `reclaim large`.
pub const DEFAULT_MIN_SIZE: u64 = 500 * 1024 * 1024;

/// Default number of files reported.
pub const DEFAULT_LIMIT: usize = 50;

/// How a large-file search should behave.
#[derive(Debug, Clone)]
pub struct LargeFileOptions {
    /// Files occupying fewer bytes on disk than this are ignored.
    pub min_size: u64,
    /// Only files not modified for at least this many days. 0 means any age.
    pub min_age_days: u32,
    /// Report at most this many, largest first. 0 means no limit.
    pub limit: usize,
    /// How deep below each root the walk descends.
    pub max_depth: usize,
    /// Descend into dot-directories (other than known artifact ones).
    pub include_hidden: bool,
    /// Whether the walk may cross onto another filesystem.
    pub cross_device: bool,
    /// Glob patterns pruned from the walk, usually `scan.exclude` from config.
    pub exclude: Vec<String>,
    /// Worker threads for the walk.
    pub threads: usize,
}

impl Default for LargeFileOptions {
    fn default() -> Self {
        Self {
            min_size: DEFAULT_MIN_SIZE,
            min_age_days: 0,
            limit: DEFAULT_LIMIT,
            max_depth: 32,
            include_hidden: false,
            cross_device: false,
            exclude: Vec::new(),
            threads: 4,
        }
    }
}

/// A coarse guess at what a file is, from its extension, so the list reads as
/// "3 videos and a VM image" rather than a wall of paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileKind {
    Video,
    Audio,
    Image,
    DiskImage,
    Archive,
    Model,
    Database,
    Log,
    Other,
}

impl FileKind {
    pub fn from_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mov" | "mkv" | "avi" | "m4v" | "webm" | "wmv" | "flv" | "mts" => {
                FileKind::Video
            }
            "wav" | "flac" | "aiff" | "aif" | "mp3" | "m4a" | "ogg" => FileKind::Audio,
            "psd" | "tif" | "tiff" | "raw" | "cr2" | "nef" | "arw" | "heic" | "exr" => {
                FileKind::Image
            }
            "dmg" | "iso" | "img" | "vmdk" | "vdi" | "qcow2" | "vhd" | "vhdx" | "sparseimage"
            | "sparsebundle" | "ipsw" => FileKind::DiskImage,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "pkg" | "xip" => {
                FileKind::Archive
            }
            "gguf" | "safetensors" | "ckpt" | "pt" | "pth" | "onnx" | "h5" => FileKind::Model,
            "sqlite" | "sqlite3" | "db" | "mdb" | "parquet" | "csv" => FileKind::Database,
            "log" | "out" => FileKind::Log,
            _ => FileKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Video => "video",
            FileKind::Audio => "audio",
            FileKind::Image => "image",
            FileKind::DiskImage => "disk image",
            FileKind::Archive => "archive",
            FileKind::Model => "model",
            FileKind::Database => "data",
            FileKind::Log => "log",
            FileKind::Other => "file",
        }
    }
}

/// One file above the threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LargeFile {
    pub path: PathBuf,
    /// Bytes actually occupied (`st_blocks`). What deleting it gives back.
    pub on_disk: u64,
    /// Apparent size (`st_size`). Much larger than `on_disk` for sparse files.
    pub logical: u64,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub kind: FileKind,
    /// Another hardlink points at the same data, so removing this path alone
    /// frees nothing.
    pub hardlinked: bool,
}

/// Result of a large-file search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LargeFileScan {
    /// Largest first, truncated to the limit.
    pub files: Vec<LargeFile>,
    /// How many files cleared the threshold before truncation.
    pub matched: usize,
    /// Bytes on disk across every match, including those cut by the limit.
    pub matched_bytes: u64,
    /// Files examined.
    pub files_scanned: u64,
    /// Directories that could not be read, so the list may be incomplete.
    pub unreadable: Vec<PathBuf>,
    pub elapsed_ms: u64,
}

/// Walk `roots` and return every regular file at or above `opts.min_size`.
pub fn find_large_files(roots: &[PathBuf], opts: &LargeFileOptions) -> LargeFileScan {
    let started = Instant::now();
    let excludes = compile_excludes(&opts.exclude);
    let found = Mutex::new(Vec::new());
    let unreadable = Mutex::new(Vec::new());
    let scanned = std::sync::atomic::AtomicU64::new(0);

    for root in roots {
        let Ok(root_meta) = std::fs::metadata(root) else {
            continue;
        };
        if root_meta.is_file() {
            // A single file named explicitly is simply measured.
            if let Some(file) = examine(root, &root_meta, opts) {
                found.lock().unwrap().push(file);
            }
            scanned.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }

        let root_device = device_of(&root_meta);
        walk_root(
            root,
            root_device,
            opts,
            &excludes,
            &found,
            &unreadable,
            &scanned,
        );
    }

    let mut files = found.into_inner().unwrap();
    files.sort_by(|a, b| b.on_disk.cmp(&a.on_disk).then_with(|| a.path.cmp(&b.path)));
    files.dedup_by(|a, b| a.path == b.path);

    let matched = files.len();
    let matched_bytes = files.iter().map(|f| f.on_disk).sum();
    if opts.limit > 0 {
        files.truncate(opts.limit);
    }

    LargeFileScan {
        files,
        matched,
        matched_bytes,
        files_scanned: scanned.into_inner(),
        unreadable: unreadable.into_inner().unwrap(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn walk_root(
    root: &Path,
    root_device: u64,
    opts: &LargeFileOptions,
    excludes: &[glob::Pattern],
    found: &Mutex<Vec<LargeFile>>,
    unreadable: &Mutex<Vec<PathBuf>>,
    scanned: &std::sync::atomic::AtomicU64,
) {
    let include_hidden = opts.include_hidden;
    let cross_device = opts.cross_device;
    let excludes_for_walk = excludes.to_vec();

    let walker = jwalk::WalkDirGeneric::<((), ())>::new(root)
        .parallelism(jwalk::Parallelism::RayonNewPool(opts.threads.max(1)))
        .skip_hidden(false)
        .follow_links(false)
        .max_depth(opts.max_depth)
        .process_read_dir(move |depth, _path, _state, children| {
            // The root itself is always walked; the prune rules apply to children.
            if depth.is_none() {
                return;
            }
            children.retain(|child| {
                let Ok(child) = child else { return true };
                if !child.file_type().is_dir() {
                    return true;
                }
                should_descend(
                    child,
                    include_hidden,
                    cross_device,
                    root_device,
                    &excludes_for_walk,
                )
            });
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if let Some(path) = e.path() {
                    unreadable.lock().unwrap().push(path.to_path_buf());
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        scanned.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let path = entry.path();
        if is_excluded(&path, excludes) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if let Some(file) = examine(&path, &meta, opts) {
            found.lock().unwrap().push(file);
        }
    }
}

fn should_descend(
    child: &jwalk::DirEntry<((), ())>,
    include_hidden: bool,
    cross_device: bool,
    root_device: u64,
    excludes: &[glob::Pattern],
) -> bool {
    let name = child.file_name().to_string_lossy();
    if NEVER_DESCEND.contains(&name.as_ref()) {
        return false;
    }
    if is_package(&child.path(), &name) {
        return false;
    }
    if !include_hidden && name.starts_with('.') {
        return false;
    }
    let path = child.path();
    if is_excluded(&path, excludes) {
        return false;
    }
    if !cross_device {
        // A mounted network share or external disk turns a scan into a hang.
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if device_of(&meta) != root_device {
                return false;
            }
        }
    }
    true
}

/// Directory extensions that mark a macOS package or an application's private
/// store. Their insides are one unit: trashing `Photos.sqlite` out of a Photos
/// library, or one blob out of a `.sparsebundle`, corrupts the whole thing.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "appex",
    "xpc",
    "photoslibrary",
    "photolibrary",
    "migratedphotolibrary",
    "aplibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "theater",
    "fcpbundle",
    "logicx",
    "band",
    "rtfd",
    "pages",
    "numbers",
    "key",
    "sparsebundle",
    "xcarchive",
    "dsym",
    "lrdata",
    "lrlibrary",
    "pvm",
    "vmwarevm",
    "utm",
    "musicdb",
];

/// Whether a directory is a package whose contents must never be offered
/// file by file. Known extensions are matched by name; any other directory
/// with an extension is treated as a bundle when it has the standard
/// `Contents/` layout.
fn is_package(path: &Path, name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || name.starts_with('.') && !name[1..].contains('.') {
        return false;
    }
    let ext = ext.to_ascii_lowercase();
    PACKAGE_EXTENSIONS.contains(&ext.as_str()) || path.join("Contents").is_dir()
}

/// Build a [`LargeFile`] when this regular file clears the threshold.
fn examine(path: &Path, meta: &std::fs::Metadata, opts: &LargeFileOptions) -> Option<LargeFile> {
    if !meta.is_file() {
        return None;
    }
    let on_disk = blocks_of(meta);
    if on_disk < opts.min_size {
        return None;
    }
    let modified = meta.modified().ok();
    if opts.min_age_days > 0 {
        // An unknown mtime cannot prove the file is old, so it is not offered.
        let age = modified.map(crate::model::days_since)?;
        if age < opts.min_age_days {
            return None;
        }
    }
    let (_, _, nlink) = identity_of(meta);
    Some(LargeFile {
        path: path.to_path_buf(),
        on_disk,
        logical: meta.len(),
        modified,
        accessed: meta.accessed().ok(),
        kind: FileKind::from_path(path),
        hardlinked: nlink > 1,
    })
}

/// Provider id carried by large-file candidates in a normal scan.
pub const PROVIDER_ID: &str = "files.large";

impl FileKind {
    /// Capitalised form for labels: "Video", "Disk image".
    pub fn title(self) -> String {
        let s = self.as_str();
        let mut chars = s.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        }
    }
}

/// Options for the large-file pass of a normal scan, from config.
pub fn scan_options(config: &Config) -> Result<LargeFileOptions> {
    let files = &config.providers.files;
    Ok(LargeFileOptions {
        min_size: files.min_size_bytes()?,
        limit: files.limit,
        include_hidden: files.include_hidden,
        cross_device: config.scan.cross_device,
        exclude: config.scan.exclude.clone(),
        threads: config.scan.threads(),
        ..LargeFileOptions::default()
    })
}

/// Offer one large file as a scan candidate.
///
/// Always caution tier with `Regen::Never`: reclaim knows nothing about where
/// else this data might exist. The `TrashFile` action means the executor moves
/// it to the Trash after a fresh guard check, whatever the delete mode says.
pub fn to_candidate(file: &LargeFile) -> Candidate {
    let name = file
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.path.display().to_string());

    CandidateBuilder::new(
        PROVIDER_ID,
        Group::Files,
        format!("{} · {name}", file.kind.title()),
    )
    .detail(format!(
        "A large {} file. Found by size alone, so reclaim cannot tell whether a copy \
             exists anywhere else. Removing it moves it to the Trash.",
        file.kind.as_str()
    ))
    .path(&file.path)
    .kind(Kind::UserFile)
    .tier(Tier::Caution)
    .action(Action::TrashFile)
    .regen(Regen::Never)
    .warn(Warning::caution(
        "Your own file: check you have a copy before removing it. It goes to the Trash.",
    ))
    .warn_if(
        file.hardlinked,
        Warning::info("Hardlinked elsewhere: removing this path frees nothing."),
    )
    .warn_if(
        file.logical > file.on_disk.saturating_mul(2),
        Warning::info(format!(
            "Sparse file: appears as {} but occupies only {} on disk.",
            crate::format::bytes(file.logical),
            crate::format::bytes(file.on_disk)
        )),
    )
    .build()
}

/// Drop large-file candidates that sit inside a directory another candidate
/// already offers (a ComfyUI `models/` folder, say), so no byte is offered twice.
pub fn exclude_covered(files: Vec<Candidate>, others: &[Candidate]) -> Vec<Candidate> {
    let covered: Vec<&Path> = others
        .iter()
        .flat_map(|c| c.paths.iter().map(PathBuf::as_path))
        .collect();
    files
        .into_iter()
        .filter(|f| {
            f.primary_path()
                .is_some_and(|p| !covered.iter().any(|dir| p.starts_with(dir)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Write a file with real (non-sparse) content so `st_blocks` is meaningful.
    fn write(base: &Path, rel: &str, bytes: usize) -> PathBuf {
        let path = base.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    fn opts(min_size: u64) -> LargeFileOptions {
        LargeFileOptions {
            min_size,
            threads: 2,
            ..LargeFileOptions::default()
        }
    }

    fn names(scan: &LargeFileScan) -> Vec<String> {
        scan.files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn finds_files_above_the_threshold_largest_first() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a/medium.mov", 300 * 1024);
        write(tmp.path(), "b/c/huge.iso", 600 * 1024);
        write(tmp.path(), "small.txt", 4 * 1024);

        let scan = find_large_files(&[tmp.path().to_path_buf()], &opts(200 * 1024));

        assert_eq!(names(&scan), vec!["huge.iso", "medium.mov"]);
        assert_eq!(scan.matched, 2);
        assert!(scan.files_scanned >= 3);
        assert_eq!(scan.files[0].kind, FileKind::DiskImage);
        assert_eq!(scan.files[1].kind, FileKind::Video);
    }

    #[test]
    fn the_limit_truncates_but_the_totals_still_count_everything() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            write(tmp.path(), &format!("f{i}.bin"), (100 + i * 10) * 1024);
        }
        let scan = find_large_files(
            &[tmp.path().to_path_buf()],
            &LargeFileOptions {
                limit: 2,
                ..opts(64 * 1024)
            },
        );
        assert_eq!(scan.files.len(), 2);
        assert_eq!(
            scan.matched, 5,
            "hidden matches must still be accounted for"
        );
        assert!(scan.matched_bytes >= scan.files.iter().map(|f| f.on_disk).sum::<u64>());
    }

    #[test]
    fn artifact_directories_are_left_to_the_provider_pipeline() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "app/node_modules/pkg/blob.bin", 300 * 1024);
        write(tmp.path(), "app/target/debug/big.rlib", 300 * 1024);
        write(tmp.path(), "app/.git/objects/pack.pack", 300 * 1024);
        write(tmp.path(), "app/recording.mp4", 300 * 1024);

        let scan = find_large_files(&[tmp.path().to_path_buf()], &opts(200 * 1024));
        assert_eq!(names(&scan), vec!["recording.mp4"]);
    }

    #[test]
    fn hidden_directories_are_skipped_unless_asked_for() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), ".ollama/models/blob.gguf", 300 * 1024);

        let default = find_large_files(&[tmp.path().to_path_buf()], &opts(200 * 1024));
        assert!(default.files.is_empty());

        let hidden = find_large_files(
            &[tmp.path().to_path_buf()],
            &LargeFileOptions {
                include_hidden: true,
                ..opts(200 * 1024)
            },
        );
        assert_eq!(names(&hidden), vec!["blob.gguf"]);
    }

    #[test]
    fn macos_packages_are_never_opened_up_file_by_file() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Pictures/Photos Library.photoslibrary/database/Photos.sqlite",
            300 * 1024,
        );
        write(tmp.path(), "Movies/Trip.imovielibrary/clip.mov", 300 * 1024);
        write(
            tmp.path(),
            "Apps/Some.Tool/Contents/Resources/big.dat",
            300 * 1024,
        );
        write(tmp.path(), "Movies/2024.trip/clip.mov", 300 * 1024);

        let scan = find_large_files(&[tmp.path().to_path_buf()], &opts(200 * 1024));
        let paths: Vec<_> = scan.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            vec![tmp.path().join("Movies/2024.trip/clip.mov")],
            "a plain folder with a dot in its name is still searched"
        );
    }

    #[test]
    fn exclude_globs_are_honoured() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "keep/a.zip", 300 * 1024);
        write(tmp.path(), "skip/b.zip", 300 * 1024);

        let scan = find_large_files(
            &[tmp.path().to_path_buf()],
            &LargeFileOptions {
                exclude: vec![format!("{}/skip", tmp.path().display())],
                ..opts(200 * 1024)
            },
        );
        assert_eq!(names(&scan), vec!["a.zip"]);
    }

    #[test]
    fn the_age_filter_hides_recently_modified_files() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "fresh.mov", 300 * 1024);
        let scan = find_large_files(
            &[tmp.path().to_path_buf()],
            &LargeFileOptions {
                min_age_days: 30,
                ..opts(200 * 1024)
            },
        );
        assert!(
            scan.files.is_empty(),
            "a file written just now is not 30 days old"
        );
    }

    #[test]
    fn a_root_that_is_a_file_is_measured_directly() {
        let tmp = TempDir::new().unwrap();
        let file = write(tmp.path(), "direct.dmg", 300 * 1024);
        let scan = find_large_files(std::slice::from_ref(&file), &opts(200 * 1024));
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].path, file);
    }

    #[test]
    fn the_same_root_twice_reports_each_file_once() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "x.iso", 300 * 1024);
        let root = tmp.path().to_path_buf();
        let scan = find_large_files(&[root.clone(), root], &opts(200 * 1024));
        assert_eq!(scan.files.len(), 1);
    }

    #[test]
    fn a_missing_root_is_skipped() {
        let scan = find_large_files(&[PathBuf::from("/definitely/not/here")], &opts(1));
        assert!(scan.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_or_reported() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(outside.path(), "real.iso", 300 * 1024);
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("real.iso"),
            tmp.path().join("file-link"),
        )
        .unwrap();

        let scan = find_large_files(&[tmp.path().to_path_buf()], &opts(200 * 1024));
        assert!(scan.files.is_empty(), "{:?}", scan.files);
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_files_are_flagged() {
        let tmp = TempDir::new().unwrap();
        let a = write(tmp.path(), "a.bin", 300 * 1024);
        fs::hard_link(&a, tmp.path().join("b.bin")).unwrap();
        let scan = find_large_files(&[a], &opts(200 * 1024));
        assert!(scan.files[0].hardlinked);
    }

    #[test]
    fn a_large_file_becomes_a_caution_candidate_that_names_its_type() {
        let file = LargeFile {
            path: PathBuf::from("/home/x/Movies/talk.mov"),
            on_disk: 900,
            logical: 900,
            modified: None,
            accessed: None,
            kind: FileKind::Video,
            hardlinked: false,
        };
        let c = to_candidate(&file);
        assert_eq!(c.label, "Video · talk.mov");
        assert_eq!(c.group, Group::Files);
        assert_eq!(c.tier, Tier::Caution);
        assert_eq!(c.action, Action::TrashFile);
        assert_eq!(c.kind, Kind::UserFile);
        assert_eq!(c.regen, Regen::Never);
        assert!(
            !c.warnings.is_empty(),
            "caution items must explain themselves"
        );
    }

    #[test]
    fn files_inside_another_candidate_are_not_offered_twice() {
        let mk = |p: &str| {
            to_candidate(&LargeFile {
                path: PathBuf::from(p),
                on_disk: 1,
                logical: 1,
                modified: None,
                accessed: None,
                kind: FileKind::Model,
                hardlinked: false,
            })
        };
        let comfy = CandidateBuilder::new("ml.comfyui", Group::Ml, "models")
            .path("/home/x/ComfyUI/models")
            .build();
        let kept = exclude_covered(
            vec![
                mk("/home/x/ComfyUI/models/sd.safetensors"),
                mk("/home/x/Downloads/a.gguf"),
            ],
            &[comfy],
        );
        assert_eq!(kept.len(), 1);
        assert!(kept[0].paths[0].ends_with("a.gguf"));
    }

    #[test]
    fn kinds_are_guessed_from_extensions_case_insensitively() {
        assert_eq!(FileKind::from_path(Path::new("x.MOV")), FileKind::Video);
        assert_eq!(FileKind::from_path(Path::new("x.gguf")), FileKind::Model);
        assert_eq!(
            FileKind::from_path(Path::new("x.tar.gz")),
            FileKind::Archive
        );
        assert_eq!(FileKind::from_path(Path::new("noext")), FileKind::Other);
    }
}
