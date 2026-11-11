//! Installed applications, and when each was last opened.
//!
//! An app's files say nothing useful about use: an auto-update rewrites the whole
//! bundle, so its mtime is "last updated", not "last used". macOS records the real
//! answer in Spotlight metadata (`kMDItemLastUsedDate`, the same date Finder's
//! "Last opened" column shows), and that is what this module reads first. Spotlight
//! misses some launches and forgets everything after a migration, so the access
//! time of the app's program file is a second witness: launching an app reads
//! it. When neither has anything, the date the app was installed stands in, and
//! the candidate says so.
//!
//! Like large files, an app is never something the tool can prove is replaceable,
//! so every app is a caution-tier candidate that only ever goes to the Trash.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::config::Config;
use crate::model::{
    days_since, Action, Candidate, CandidateBuilder, Group, Kind, Regen, Tier, Warning,
};

/// Provider id for apps offered in a normal scan. The `apps` prefix is the group
/// key, so `disabled = ["apps"]` turns the whole pass off.
pub const PROVIDER_ID: &str = "apps.unused";

/// Apps opened more recently than this are not offered in a scan.
pub const DEFAULT_UNUSED_DAYS: u32 = 90;

/// How many bundle paths go to one `mdls` invocation, well under `ARG_MAX`.
const MDLS_BATCH: usize = 200;

/// What the OS knows about when an app was used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// `kMDItemLastUsedDate`.
    pub last_opened: Option<SystemTime>,
    /// `kMDItemDateAdded`: when the bundle arrived in its folder.
    pub added: Option<SystemTime>,
}

/// Looks up [`Usage`] for a batch of bundles, one result per path, in order.
/// Injected so tests never depend on the machine's Spotlight index.
pub type UsageLookup = dyn Fn(&[PathBuf]) -> Vec<Usage> + Sync;

/// One installed application.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstalledApp {
    pub path: PathBuf,
    /// The bundle name without `.app`.
    pub name: String,
    /// When the app was last opened, if the OS recorded it.
    pub last_opened: Option<SystemTime>,
    /// When it was installed: Spotlight's date added, else the bundle's own mtime.
    pub installed: Option<SystemTime>,
    /// Owned by another user (normally root, for App Store and `.pkg` installs),
    /// so moving it to the Trash needs an administrator password.
    pub system_owned: bool,
}

impl InstalledApp {
    /// The best available "last used" moment: opened if known, else installed.
    pub fn last_activity(&self) -> Option<SystemTime> {
        self.last_opened.or(self.installed)
    }

    /// Whole days since [`Self::last_activity`], if anything is known.
    pub fn idle_days(&self) -> Option<u32> {
        self.last_activity().map(days_since)
    }
}

/// Every `.app` bundle directly inside `dirs`, or one folder down
/// (`/Applications/Adobe Acrobat DC/Adobe Acrobat.app`). Symlinked bundles are
/// skipped: `/Applications/Safari.app` is a link into the sealed system volume.
/// So are browser web-app shortcuts, which are not apps.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| {
            entries(dir).into_iter().flat_map(|entry| {
                if is_bundle(&entry) {
                    vec![entry]
                } else {
                    entries(&entry)
                        .into_iter()
                        .filter(|e| is_bundle(e))
                        .collect()
                }
            })
        })
        .filter(|bundle| !is_web_app_shortcut(bundle))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Real (non-symlink) subdirectories of `dir`. Unreadable means empty.
fn entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// Chrome-family browsers (Chrome, Chromium, Edge, Brave, Comet) write a tiny
/// launcher bundle per installed web app into `~/Applications/<Browser> Apps`.
/// They are the browser's to manage, not apps, and the key below is in every one.
const WEB_APP_SHORTCUT_KEY: &[u8] = b"CrAppModeShortcutID";

/// Real `Info.plist` files are a few KB; never read an absurd one into memory.
const MAX_INFO_PLIST_BYTES: u64 = 1024 * 1024;

fn is_web_app_shortcut(bundle: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(bundle.join("Contents/Info.plist")) else {
        return false;
    };
    let mut plist = Vec::new();
    if file
        .take(MAX_INFO_PLIST_BYTES)
        .read_to_end(&mut plist)
        .is_err()
    {
        return false;
    }
    plist
        .windows(WEB_APP_SHORTCUT_KEY.len())
        .any(|w| w == WEB_APP_SHORTCUT_KEY)
}

fn is_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

/// Find every installed app and attach its usage dates.
pub fn list_apps(dirs: &[PathBuf], lookup: &UsageLookup) -> Vec<InstalledApp> {
    let bundles = find_bundles(dirs);
    let usage = lookup(&bundles);
    bundles
        .into_iter()
        .enumerate()
        .map(|(i, path)| describe(path, usage.get(i).copied().unwrap_or_default()))
        .collect()
}

fn describe(path: PathBuf, usage: Usage) -> InstalledApp {
    let meta = std::fs::symlink_metadata(&path).ok();
    let installed = usage
        .added
        .or_else(|| meta.as_ref().and_then(|m| m.modified().ok()));
    // Spotlight misses launches (login items, apps started from a terminal) and
    // forgets everything after a migration. Launching an app reads its program
    // file, so that file's access time is a second witness; take the later one.
    let last_opened = usage.last_opened.max(last_launched(&path));
    InstalledApp {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        last_opened,
        installed,
        system_owned: meta.as_ref().is_some_and(owned_by_someone_else),
        path,
    }
}

/// An access this soon after a file was written is the installer or Gatekeeper
/// checking it, not the user launching it.
const INSTALL_READ_GRACE: Duration = Duration::from_secs(5 * 60);

/// The newest access time among the program files in `Contents/MacOS`, ignoring
/// reads that happened while the app was being installed or updated.
fn last_launched(bundle: &Path) -> Option<SystemTime> {
    let read = std::fs::read_dir(bundle.join("Contents/MacOS")).ok()?;
    read.flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .filter_map(|meta| {
            let accessed = meta.accessed().ok()?;
            let written = meta.modified().ok()?;
            (accessed > written + INSTALL_READ_GRACE).then_some(accessed)
        })
        .max()
}

#[cfg(unix)]
fn owned_by_someone_else(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    meta.uid() != unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn owned_by_someone_else(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Bytes on disk for each app, in order, measured in parallel. Hardlinks shared
/// between bundles are counted once.
pub fn sizes(apps: &[InstalledApp]) -> Vec<u64> {
    use rayon::prelude::*;
    let links = crate::walk::LinkTracker::new();
    // Serial walks, parallel across apps. A nested walk on the global pool
    // would wait for workers this very `par_iter` is holding, time out, and
    // report nothing.
    let opts = crate::walk::WalkOptions {
        threads: 1,
        ..crate::walk::WalkOptions::default()
    };
    apps.par_iter()
        .map(|app| crate::walk::measure(&app.path, &links, &opts).size.on_disk)
        .collect()
}

/// Whether any running process's executable lives inside `bundle`. Errors
/// reading the process list count as "not running": the Trash move is
/// reversible, and Finder warns about running apps itself.
pub fn is_running(bundle: &Path) -> bool {
    let Ok(out) = Command::new("ps").args(["-axo", "comm="]).output() else {
        return false;
    };
    let prefix = format!("{}/", bundle.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

/// Read usage dates from Spotlight with `mdls`. Any failure (no `mdls`, a
/// malformed reply) yields "unknown" for the whole batch rather than a guess.
pub fn spotlight_usage(paths: &[PathBuf]) -> Vec<Usage> {
    paths
        .chunks(MDLS_BATCH)
        .flat_map(|batch| {
            let opened = mdls_dates(batch, "kMDItemLastUsedDate");
            let added = mdls_dates(batch, "kMDItemDateAdded");
            (0..batch.len()).map(move |i| Usage {
                last_opened: opened.get(i).copied().flatten(),
                added: added.get(i).copied().flatten(),
            })
        })
        .collect()
}

fn mdls_dates(paths: &[PathBuf], attribute: &str) -> Vec<Option<SystemTime>> {
    if paths.is_empty() || !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let output = Command::new("mdls")
        .args(["-raw", "-nullMarker", "", "-name", attribute])
        .args(paths)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_mdls_raw(&String::from_utf8_lossy(&out.stdout), paths.len())
        }
        Ok(out) => {
            tracing::debug!("mdls exited with {}", out.status);
            Vec::new()
        }
        Err(e) => {
            tracing::debug!("could not run mdls: {e}");
            Vec::new()
        }
    }
}

/// `mdls -raw` separates values with NUL. A reply with the wrong number of
/// values cannot be lined up with the paths, so it is discarded whole.
fn parse_mdls_raw(stdout: &str, expected: usize) -> Vec<Option<SystemTime>> {
    let values: Vec<&str> = stdout.split('\0').collect();
    let values = match values.as_slice() {
        // A trailing NUL leaves one empty extra field.
        [rest @ .., ""] if rest.len() == expected => rest,
        all if all.len() == expected => all,
        _ => return Vec::new(),
    };
    values.iter().map(|v| parse_mdls_date(v)).collect()
}

/// Parse `2026-10-13 04:38:32 +0000`, the only date format `mdls` prints.
fn parse_mdls_date(raw: &str) -> Option<SystemTime> {
    let raw = raw.trim();
    let (stamp, offset) = match raw.rsplit_once(' ') {
        Some((stamp, zone)) if zone.starts_with(['+', '-']) => (stamp, zone),
        _ => (raw, "+0000"),
    };
    let utc_naive = humantime::parse_rfc3339_weak(stamp).ok()?;
    let sign: i64 = if offset.starts_with('-') { -1 } else { 1 };
    let digits = offset.get(1..5)?;
    let hours: i64 = digits.get(0..2)?.parse().ok()?;
    let minutes: i64 = digits.get(2..4)?.parse().ok()?;
    let shift = sign * (hours * 3600 + minutes * 60);
    // Local time = UTC + offset, so UTC = local - offset.
    if shift >= 0 {
        utc_naive.checked_sub(Duration::from_secs(shift as u64))
    } else {
        utc_naive.checked_add(Duration::from_secs((-shift) as u64))
    }
}

/// The scan-time view: installed apps idle for at least the configured number of
/// days, as candidates.
pub fn discover(app_dirs: &[PathBuf], config: &Config, lookup: &UsageLookup) -> Vec<Candidate> {
    let unused_days = config.providers.apps.unused_days;
    list_apps(app_dirs, lookup)
        .iter()
        .filter(|app| app.idle_days().is_some_and(|d| d >= unused_days))
        .map(to_candidate)
        .collect()
}

/// Present an app as a caution-tier, Trash-only candidate.
pub fn to_candidate(app: &InstalledApp) -> Candidate {
    let label = if app.last_opened.is_some() {
        app.name.clone()
    } else {
        format!("{} · never opened", app.name)
    };
    CandidateBuilder::new(PROVIDER_ID, Group::Apps, label)
        .detail(detail(app))
        .path(app.path.clone())
        .kind(Kind::Application)
        .tier(Tier::Caution)
        .action(Action::TrashApp)
        .regen(Regen::Download {
            bytes: None,
            source: "the App Store or the developer's website".into(),
        })
        .last_opened(app.last_activity())
        .warn(Warning::caution(
            "Reinstalling means downloading it again, and signing in or re-entering a licence if it needs one.",
        ))
        .warn_if(
            app.last_opened.is_none(),
            Warning::info(
                "No record of this app ever being opened, so its age is counted from when it was installed.",
            ),
        )
        .warn_if(
            app.system_owned,
            Warning::info(
                "Installed for all users (App Store or an installer), so macOS will ask for your password to move it.",
            ),
        )
        .build()
}

fn detail(app: &InstalledApp) -> String {
    let when = match (app.last_opened, app.installed) {
        (Some(opened), _) => format!("Last opened {}.", describe_date(opened)),
        (None, Some(installed)) => format!(
            "No record of it being opened; installed {}.",
            describe_date(installed)
        ),
        (None, None) => "No record of when it was installed or opened.".to_string(),
    };
    format!(
        "{when} Removing it moves the app to the Trash. Its settings and data in ~/Library are left alone."
    )
}

/// `2026-03-02 (8 months ago)`.
pub fn describe_date(when: SystemTime) -> String {
    let date = humantime::format_rfc3339_seconds(when).to_string();
    let day = date.get(..10).unwrap_or(&date);
    format!("{day} ({})", crate::model::humanize_age(days_since(when)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const DAY: u64 = 86_400;

    fn ago(days: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days * DAY)
    }

    fn bundle(base: &Path, rel: &str) -> PathBuf {
        let path = base.join(rel);
        fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        fs::write(path.join("Contents/MacOS/bin"), b"x").unwrap();
        path
    }

    /// A lookup that knows a fixed last-opened date per bundle name.
    fn lookup_from(known: &'static [(&'static str, u64)]) -> impl Fn(&[PathBuf]) -> Vec<Usage> {
        move |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| {
                    let name = p.file_stem().unwrap().to_string_lossy().into_owned();
                    let opened = known.iter().find(|(n, _)| *n == name).map(|(_, d)| ago(*d));
                    Usage {
                        last_opened: opened,
                        added: Some(ago(400)),
                    }
                })
                .collect()
        }
    }

    fn config(unused_days: u32) -> Config {
        let mut config = Config::default();
        config.providers.apps.unused_days = unused_days;
        config
    }

    #[test]
    fn finds_bundles_at_the_top_level_and_one_folder_down() {
        let tmp = TempDir::new().unwrap();
        bundle(tmp.path(), "Top.app");
        bundle(tmp.path(), "Suite/Nested.app");
        bundle(tmp.path(), "Suite/Deeper/TooDeep.app");
        fs::create_dir_all(tmp.path().join("NotAnApp")).unwrap();

        let names: Vec<String> = find_bundles(&[tmp.path().to_path_buf()])
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Nested.app", "Top.app"]);
    }

    #[test]
    fn helpers_inside_a_bundle_are_not_listed_separately() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Big.app");
        bundle(&app, "Contents/Helpers/Helper.app");

        let found = find_bundles(&[tmp.path().to_path_buf()]);
        assert_eq!(found, [app]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_bundles_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let real = bundle(tmp.path(), "elsewhere/Real.app");
        let apps = tmp.path().join("Applications");
        fs::create_dir_all(&apps).unwrap();
        std::os::unix::fs::symlink(&real, apps.join("Linked.app")).unwrap();

        assert!(find_bundles(&[apps]).is_empty());
    }

    #[test]
    fn browser_web_app_shortcuts_are_left_out() {
        let tmp = TempDir::new().unwrap();
        let real = bundle(tmp.path(), "Editor.app");
        fs::write(
            real.join("Contents/Info.plist"),
            "<plist><dict><key>CFBundleName</key><string>Editor</string></dict></plist>",
        )
        .unwrap();
        let shim = bundle(tmp.path(), "Chrome Apps.localized/Grok.app");
        fs::write(
            shim.join("Contents/Info.plist"),
            "<plist><dict><key>CrAppModeShortcutID</key><string>abc</string></dict></plist>",
        )
        .unwrap();

        assert_eq!(find_bundles(&[tmp.path().to_path_buf()]), [real]);
    }

    #[test]
    fn a_missing_folder_contributes_nothing() {
        let tmp = TempDir::new().unwrap();
        assert!(find_bundles(&[tmp.path().join("nope")]).is_empty());
    }

    #[test]
    fn list_attaches_usage_and_falls_back_to_the_install_date() {
        let tmp = TempDir::new().unwrap();
        bundle(tmp.path(), "Used.app");
        bundle(tmp.path(), "Never.app");

        let apps = list_apps(&[tmp.path().to_path_buf()], &lookup_from(&[("Used", 10)]));
        let never = apps.iter().find(|a| a.name == "Never").unwrap();
        let used = apps.iter().find(|a| a.name == "Used").unwrap();

        assert!(never.last_opened.is_none());
        assert_eq!(never.idle_days(), Some(400));
        assert_eq!(used.idle_days(), Some(10));
        assert!(!used.system_owned, "a bundle we created is ours");
    }

    #[test]
    fn without_any_spotlight_data_the_bundle_mtime_is_the_install_date() {
        let tmp = TempDir::new().unwrap();
        bundle(tmp.path(), "Fresh.app");

        let apps = list_apps(&[tmp.path().to_path_buf()], &|p: &[PathBuf]| {
            vec![Usage::default(); p.len()]
        });
        assert_eq!(apps[0].idle_days(), Some(0));
    }

    fn set_times(file: &Path, written: SystemTime, read: SystemTime) {
        let times = fs::FileTimes::new()
            .set_modified(written)
            .set_accessed(read);
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(times)
            .unwrap();
    }

    fn no_spotlight() -> impl Fn(&[PathBuf]) -> Vec<Usage> {
        |p: &[PathBuf]| vec![Usage::default(); p.len()]
    }

    #[test]
    fn a_program_read_long_after_install_counts_as_a_launch() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Launched.app");
        set_times(&app.join("Contents/MacOS/bin"), ago(300), ago(40));

        let apps = list_apps(&[tmp.path().to_path_buf()], &no_spotlight());
        assert_eq!(apps[0].idle_days(), Some(40));
        assert!(apps[0].last_opened.is_some());
    }

    #[test]
    fn a_program_read_while_installing_is_not_a_launch() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Unopened.app");
        let installed = ago(300);
        set_times(
            &app.join("Contents/MacOS/bin"),
            installed,
            installed + Duration::from_secs(10),
        );

        let apps = list_apps(&[tmp.path().to_path_buf()], &no_spotlight());
        assert!(apps[0].last_opened.is_none());
    }

    #[test]
    fn the_later_of_spotlight_and_the_program_read_wins() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Both.app");
        set_times(&app.join("Contents/MacOS/bin"), ago(300), ago(20));

        let spotlight_older =
            list_apps(&[tmp.path().to_path_buf()], &lookup_from(&[("Both", 100)]));
        assert_eq!(spotlight_older[0].idle_days(), Some(20));
        let spotlight_newer = list_apps(&[tmp.path().to_path_buf()], &lookup_from(&[("Both", 3)]));
        assert_eq!(spotlight_newer[0].idle_days(), Some(3));
    }

    #[test]
    fn a_short_lookup_reply_means_unknown_not_a_crash() {
        let tmp = TempDir::new().unwrap();
        bundle(tmp.path(), "A.app");
        bundle(tmp.path(), "B.app");

        let apps = list_apps(&[tmp.path().to_path_buf()], &|_: &[PathBuf]| Vec::new());
        assert_eq!(apps.len(), 2);
        assert!(apps.iter().all(|a| a.last_opened.is_none()));
    }

    #[test]
    fn only_apps_idle_past_the_threshold_are_offered() {
        let tmp = TempDir::new().unwrap();
        bundle(tmp.path(), "Daily.app");
        bundle(tmp.path(), "Forgotten.app");

        let found = discover(
            &[tmp.path().to_path_buf()],
            &config(90),
            &lookup_from(&[("Daily", 1), ("Forgotten", 200)]),
        );
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Forgotten"]);
    }

    #[test]
    fn an_app_becomes_a_caution_trash_only_candidate_carrying_its_last_opened_date() {
        let opened = ago(200);
        let app = InstalledApp {
            path: PathBuf::from("/Applications/Old.app"),
            name: "Old".into(),
            last_opened: Some(opened),
            installed: Some(ago(500)),
            system_owned: true,
        };
        let c = to_candidate(&app);

        assert_eq!(c.provider, PROVIDER_ID);
        assert_eq!(c.group, Group::Apps);
        assert_eq!(c.kind, Kind::Application);
        assert_eq!(c.tier, Tier::Caution);
        assert_eq!(c.action, Action::TrashApp);
        assert_eq!(c.label, "Old");
        assert_eq!(c.last_opened, Some(opened));
        assert!(c.detail.starts_with("Last opened "), "{}", c.detail);
        assert!(c.warnings.iter().any(|w| w.message.contains("password")));
    }

    #[test]
    fn a_never_opened_app_says_so_in_its_label() {
        let app = InstalledApp {
            path: PathBuf::from("/Applications/Trial.app"),
            name: "Trial".into(),
            last_opened: None,
            installed: Some(ago(300)),
            system_owned: false,
        };
        let c = to_candidate(&app);

        assert_eq!(c.label, "Trial · never opened");
        assert_eq!(c.last_opened, app.installed);
        assert!(c.detail.contains("installed"), "{}", c.detail);
        assert!(c.warnings.iter().any(|w| w.message.contains("No record")));
    }

    #[test]
    fn mdls_dates_parse_in_utc_and_with_an_offset() {
        let utc = parse_mdls_date("2026-10-13 04:38:32 +0000").unwrap();
        let shifted = parse_mdls_date("2026-10-13 06:38:32 +0200").unwrap();
        assert_eq!(utc, shifted);
        assert_eq!(
            humantime::format_rfc3339_seconds(utc).to_string(),
            "2026-10-13T04:38:32Z"
        );
        assert!(parse_mdls_date("").is_none());
        assert!(parse_mdls_date("(null)").is_none());
    }

    #[test]
    fn mdls_raw_output_lines_up_with_the_paths_or_is_discarded() {
        let reply = ["2026-10-13 04:38:32 +0000", "", "2025-01-01 00:00:00 +0000"].join("\0");
        let parsed = parse_mdls_raw(&reply, 3);
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_some() && parsed[1].is_none() && parsed[2].is_some());

        assert_eq!(
            parse_mdls_raw("a\0b\0", 2).len(),
            2,
            "trailing NUL tolerated"
        );
        assert!(
            parse_mdls_raw(&reply, 2).is_empty(),
            "miscount is discarded"
        );
    }

    #[test]
    fn sizes_line_up_with_the_apps() {
        let tmp = TempDir::new().unwrap();
        let small = bundle(tmp.path(), "Small.app");
        let big = bundle(tmp.path(), "Big.app");
        fs::write(big.join("Contents/blob"), vec![0u8; 64 * 1024]).unwrap();
        let apps = list_apps(&[tmp.path().to_path_buf()], &|p: &[PathBuf]| {
            vec![Usage::default(); p.len()]
        });
        assert_eq!(apps[0].path, big);
        assert_eq!(apps[1].path, small);

        let sizes = sizes(&apps);
        assert!(sizes[0] >= 64 * 1024 && sizes[0] > sizes[1], "{sizes:?}");
    }

    #[test]
    fn a_bundle_nothing_runs_from_is_not_running() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Idle.app");
        assert!(!is_running(&app));
    }

    /// macOS only: Linux `ps` reports a program's name, not its path, and
    /// application bundles only exist on macOS anyway.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_launched_from_inside_a_bundle_counts_as_running() {
        let tmp = TempDir::new().unwrap();
        let app = bundle(tmp.path(), "Busy.app");
        // A symlink, not a copy: a copied system binary fails its code-signing
        // check and is killed at launch. `ps` reports the path it was run by.
        let exe = app.join("Contents/MacOS/sleep");
        std::os::unix::fs::symlink("/bin/sleep", &exe).unwrap();
        let mut child = Command::new(&exe).arg("30").spawn().unwrap();

        let running = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            is_running(&app)
        });
        child.kill().unwrap();
        let _ = child.wait();
        assert!(running);
    }

    #[test]
    fn dates_are_described_with_a_day_and_an_age() {
        let text = describe_date(ago(3));
        let (day, age) = text.split_at(10);
        assert_eq!(day.as_bytes()[4], b'-');
        assert_eq!(day.as_bytes()[7], b'-');
        assert_eq!(age, " (3 days ago)");
    }
}
