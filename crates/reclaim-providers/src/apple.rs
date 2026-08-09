//! Apple: Xcode derived data, device support, archives, simulators, SwiftPM, CocoaPods.
//!
//! These are the biggest wins on a macOS developer machine and also contain the
//! single most dangerous item in the whole tool: Xcode archives hold the dSYMs
//! needed to symbolicate production crash reports, and they exist nowhere else.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

pub struct XcodeArtifacts;

impl Provider for XcodeArtifacts {
    fn id(&self) -> &'static str {
        "apple.xcode"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let Some(developer) = ctx.paths.xcode_dir() else {
            return Vec::new(); // not macOS
        };
        let mut out = Vec::new();

        if let Some(c) = global_cache(
            ctx,
            "apple.derived-data",
            Group::Apple,
            "Xcode DerivedData",
            developer.join("Xcode/DerivedData"),
        ) {
            out.push(
                c.detail("Intermediate build products, indexes and module caches for every project you have opened.")
                    .regen(Regen::Rebuild { minutes: 6 })
                    .warn(Warning::info(
                        "Xcode rebuilds this on the next build; the first build afterwards is slow.",
                    ))
                    .build(),
            );
        }

        // Device support is per-iOS-version symbol data, copied off a device the
        // first time it is connected. Big, and awkward to get back.
        for platform in ["iOS", "watchOS", "tvOS", "visionOS"] {
            let dir = developer.join(format!("Xcode/{platform} DeviceSupport"));
            if !dir.is_dir() {
                continue;
            }
            let versions = count_entries(&dir);
            out.push(
                CandidateBuilder::new("apple.device-support", Group::Apple, format!("{platform} DeviceSupport"))
                    .path(dir)
                    .detail(format!("Symbol data for {versions} {platform} version(s), copied from devices you have connected."))
                    .tier(Tier::Review)
                    .regen(Regen::Rebuild { minutes: 10 })
                    .warn(Warning::caution(
                        "Regenerated only when a device running that OS version is next connected, \
                         which is slow and needs the physical device.",
                    ))
                    .build(),
            );
        }

        // The one genuinely irreplaceable item in the Apple ecosystem.
        let archives = developer.join("Xcode/Archives");
        if archives.is_dir() {
            let count = subdirs(&archives).len();
            out.push(
                CandidateBuilder::new("apple.archives", Group::Apple, "Xcode Archives")
                    .path(archives)
                    .detail("Archived builds, including the dSYM symbol files for releases you have shipped.")
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(Warning::danger(format!(
                        "Contains {count} archive group(s) with the dSYMs needed to symbolicate \
                         crash reports from shipped builds. These cannot be regenerated from source.",
                    )))
                    .build(),
            );
        }

        for (id, label, path, detail) in [
            (
                "apple.swiftpm-cache",
                "SwiftPM cache",
                ctx.paths.cache_dir().join("org.swift.swiftpm"),
                "Cloned and resolved Swift package dependencies.",
            ),
            (
                "apple.xcode-caches",
                "Xcode caches",
                ctx.paths.cache_dir().join("com.apple.dt.Xcode"),
                "Xcode's own scratch caches.",
            ),
            (
                "apple.simulator-caches",
                "Simulator caches",
                ctx.paths
                    .home_join("Library/Developer/CoreSimulator/Caches"),
                "Simulator runtime scratch data.",
            ),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Apple, label, path) {
                out.push(c.detail(detail).regen(automatic("the next build")).build());
            }
        }

        if let Some(c) = global_cache(
            ctx,
            "apple.cocoapods-cache",
            Group::Apple,
            "CocoaPods cache",
            ctx.paths.cache_dir().join("CocoaPods"),
        ) {
            out.push(c.regen(redownload("the CocoaPods CDN")).build());
        }

        // Device backups regularly account for 20 GB+ on an iOS developer's
        // machine and are easy to forget about. They are personal data rather
        // than a build artifact, so each backup is offered individually and
        // never bulk-selected.
        let backups = ctx
            .paths
            .home_join("Library/Application Support/MobileSync/Backup");
        if backups.is_dir() {
            for backup in subdirs(&backups) {
                let name = backup
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let short = name.chars().take(12).collect::<String>();
                out.push(
                    CandidateBuilder::new(
                        "apple.device-backup",
                        Group::Apple,
                        format!("iPhone/iPad backup {short}…"),
                    )
                    .path(backup)
                    .detail("A full local backup of an iOS device: photos, messages and app data.")
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(Warning::danger(
                        "This is your device's data, not a cache. If the device is gone or was \
                         never backed up elsewhere, deleting this loses it permanently. Prefer \
                         Finder → Manage Backups so you can see which device each one belongs to.",
                    ))
                    .build(),
                );
            }
        }

        out
    }
}

pub struct SimulatorRuntimes;

impl Provider for SimulatorRuntimes {
    fn id(&self) -> &'static str {
        "apple.simulators"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        if !cfg!(target_os = "macos") || !has_command("xcrun") {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Orphaned simulator devices belong to runtimes that are no longer
        // installed, so removing them can never break a working setup.
        out.push(
            CandidateBuilder::new("apple.simulators", Group::Apple, "Unavailable simulator devices")
                .path(ctx.paths.home_join("Library/Developer/CoreSimulator/Devices"))
                .detail(
                    "Simulator devices whose runtime is no longer installed. `simctl delete unavailable` \
                     removes exactly these and nothing else.",
                )
                .action(reclaim_core::model::Action::Shell {
                    program: "xcrun".into(),
                    args: vec!["simctl".into(), "delete".into(), "unavailable".into()],
                })
                .tier(Tier::Safe)
                .regen(Regen::Automatic { on: "recreating a simulator".into() })
                .warn(Warning::info(
                    "Only removes devices whose runtime is already gone; working simulators are untouched.",
                ))
                .build(),
        );

        // The devices directory itself: each simulator holds installed apps and
        // their data, which is real state a developer may be mid-debugging.
        let devices = ctx
            .paths
            .home_join("Library/Developer/CoreSimulator/Devices");
        if devices.is_dir() {
            let count = subdirs(&devices).len();
            out.push(
                CandidateBuilder::new("apple.simulator-devices", Group::Apple, "All simulator devices")
                    .path(devices)
                    .detail(format!("{count} simulator device(s), including installed apps and their data."))
                    .tier(Tier::Caution)
                    .regen(Regen::Rebuild { minutes: 5 })
                    .warn(Warning::danger(
                        "Deletes app data, logins and databases inside every simulator, not just the stale ones.",
                    ))
                    .build(),
            );
        }

        out
    }
}

pub struct CocoaPods;

impl Provider for CocoaPods {
    fn id(&self) -> &'static str {
        "apple.pods"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["Podfile", "Package.swift", "Cartfile"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        artifacts_in_projects(ctx, &["Podfile", "Package.swift", "Cartfile"], |project| {
            let mut out = Vec::new();

            if project.has_marker("Podfile") {
                let has_lock = project.path.join("Podfile.lock").is_file();
                if let Some(c) = project_artifact(
                    "apple.pods",
                    Group::Apple,
                    "Pods/",
                    &project.path,
                    project.path.join("Pods"),
                ) {
                    out.push(
                        c.detail("Installed CocoaPods dependencies.")
                            .tier(if has_lock { Tier::Safe } else { Tier::Caution })
                            .regen(if has_lock {
                                redownload("the CocoaPods CDN")
                            } else {
                                Regen::Never
                            })
                            .warn_if(!has_lock, no_lockfile_warning("pod install"))
                            .build(),
                    );
                }
            }

            for (dir, label) in [
                ("Carthage/Build", "Carthage build output"),
                (".build", "SwiftPM build output"),
                ("DerivedData", "project DerivedData"),
            ] {
                if let Some(c) = project_artifact(
                    "apple.project-build",
                    Group::Apple,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(Regen::Rebuild { minutes: 4 }).build());
                }
            }

            out
        })
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(XcodeArtifacts),
        Box::new(SimulatorRuntimes),
        Box::new(CocoaPods),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "macOS-only paths")]
    fn derived_data_is_safe_and_rebuildable() {
        let home = TestHome::new();
        home.file(
            "Library/Developer/Xcode/DerivedData/App-abc/Build/x.o",
            8192,
        );

        let found = home.discover(&XcodeArtifacts);
        let dd = found
            .iter()
            .find(|c| c.provider == "apple.derived-data")
            .expect("DerivedData");
        assert_eq!(dd.tier, Tier::Safe);
        assert!(matches!(dd.regen, Regen::Rebuild { .. }));
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "macOS-only paths")]
    fn archives_are_caution_and_explain_the_dsym_risk() {
        // The single most dangerous thing this tool can offer to delete.
        let home = TestHome::new();
        home.file(
            "Library/Developer/Xcode/Archives/2026-01-01/App.xcarchive/Info.plist",
            512,
        );

        let found = home.discover(&XcodeArtifacts);
        let archives = found
            .iter()
            .find(|c| c.provider == "apple.archives")
            .expect("Archives");
        assert_eq!(archives.tier, Tier::Caution);
        assert_eq!(archives.regen, Regen::Never);
        let warning = &archives.warnings[0];
        assert_eq!(warning.severity, reclaim_core::Severity::Danger);
        assert!(warning.message.contains("dSYM"), "{}", warning.message);
        assert!(
            warning.message.contains("symbolicate"),
            "{}",
            warning.message
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "macOS-only paths")]
    fn device_support_warns_that_a_physical_device_is_needed() {
        let home = TestHome::new();
        home.file(
            "Library/Developer/Xcode/iOS DeviceSupport/18.0/Symbols/x",
            4096,
        );

        let found = home.discover(&XcodeArtifacts);
        let ds = found
            .iter()
            .find(|c| c.provider == "apple.device-support")
            .expect("DeviceSupport");
        assert_eq!(ds.tier, Tier::Review);
        assert!(ds
            .warnings
            .iter()
            .any(|w| w.message.contains("physical device")));
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "macOS-only paths")]
    fn device_backups_are_offered_individually_and_marked_as_personal_data() {
        let home = TestHome::new();
        home.file(
            "Library/Application Support/MobileSync/Backup/00008030-ABC/Manifest.db",
            4096,
        );
        home.file(
            "Library/Application Support/MobileSync/Backup/00008030-XYZ/Manifest.db",
            4096,
        );

        let found = home.discover(&XcodeArtifacts);
        let backups: Vec<_> = found
            .iter()
            .filter(|c| c.provider == "apple.device-backup")
            .collect();

        assert_eq!(
            backups.len(),
            2,
            "one candidate per device, never a single bulk item"
        );
        for backup in backups {
            assert_eq!(backup.tier, Tier::Caution);
            assert_eq!(backup.regen, Regen::Never);
            assert!(backup
                .warnings
                .iter()
                .any(|w| w.message.contains("not a cache")));
        }
    }

    #[test]
    fn nothing_is_offered_on_a_machine_without_xcode() {
        let home = TestHome::new();
        assert!(home.discover(&XcodeArtifacts).is_empty());
    }

    #[test]
    fn pods_with_a_lockfile_are_safe_and_re_downloadable() {
        let home = TestHome::new();
        home.project("dev/ios-app", &["Podfile", "Podfile.lock"]);
        home.file("dev/ios-app/Pods/Alamofire/Source.swift", 4096);

        let found = home.discover(&CocoaPods);
        let pods = found
            .iter()
            .find(|c| c.provider == "apple.pods")
            .expect("Pods");
        assert_eq!(pods.tier, Tier::Safe);
        assert!(matches!(pods.regen, Regen::Download { .. }));
    }

    #[test]
    fn pods_without_a_lockfile_are_caution() {
        let home = TestHome::new();
        home.project("dev/ios-app", &["Podfile"]);
        home.file("dev/ios-app/Pods/Alamofire/Source.swift", 4096);

        let found = home.discover(&CocoaPods);
        let pods = found
            .iter()
            .find(|c| c.provider == "apple.pods")
            .expect("Pods");
        assert_eq!(pods.tier, Tier::Caution);
    }

    #[test]
    fn simulator_devices_warn_about_losing_app_data() {
        let home = TestHome::new();
        home.dir("Library/Developer/CoreSimulator/Devices/ABC-123/data");

        let found = home.discover(&SimulatorRuntimes);
        if let Some(devices) = found
            .iter()
            .find(|c| c.provider == "apple.simulator-devices")
        {
            assert_eq!(devices.tier, Tier::Caution);
            assert!(devices
                .warnings
                .iter()
                .any(|w| w.message.contains("app data")));
        }
    }
}
