//! Android: SDK system images, AVDs, NDK versions and build caches.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

pub struct AndroidSdk;

impl Provider for AndroidSdk {
    fn id(&self) -> &'static str {
        "android.sdk"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let Some(sdk) = ctx.paths.android_sdk() else {
            return Vec::new();
        };
        if !sdk.is_dir() {
            return Vec::new();
        }
        let mut out = Vec::new();

        // System images are the biggest single item in a typical Android SDK,
        // several GB per API level, and re-downloadable via the SDK Manager.
        let images = sdk.join("system-images");
        if images.is_dir() {
            for api in subdirs(&images) {
                let name = api
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                out.push(
                    CandidateBuilder::new(
                        "android.system-image",
                        Group::Android,
                        format!("system image: {name}"),
                    )
                    .path(api)
                    .detail("Emulator system image for one API level.")
                    .tier(Tier::Review)
                    .regen(redownload("the Android SDK Manager"))
                    .warn(Warning::caution(
                        "Any AVD using this image will not start until the image is reinstalled.",
                    ))
                    .build(),
                );
            }
        }

        // Old NDK versions accumulate; each is several GB.
        let ndk = sdk.join("ndk");
        if ndk.is_dir() {
            let versions = subdirs(&ndk);
            if versions.len() > 1 {
                for version in versions {
                    let name = version
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    out.push(
                        CandidateBuilder::new("android.ndk", Group::Android, format!("NDK {name}"))
                            .path(version)
                            .tier(Tier::Review)
                            .regen(redownload("the Android SDK Manager"))
                            .warn(Warning::caution(format!(
                                "A project pinning NDK {name} in its Gradle config will fail to build \
                                 until it is reinstalled."
                            )))
                            .build(),
                    );
                }
            }
        }

        for (id, label, rel) in [
            (
                "android.build-cache",
                "Android build cache",
                ".android/build-cache",
            ),
            ("android.cache", "Android SDK cache", ".android/cache"),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Android, label, ctx.paths.home_join(rel))
            {
                out.push(c.regen(automatic("the next build")).build());
            }
        }

        // Old emulator temp state, safe to remove.
        if let Some(c) = global_cache(
            ctx,
            "android.emulator-tmp",
            Group::Android,
            "Emulator temp files",
            ctx.paths.home_join(".android/tmp"),
        ) {
            out.push(c.build());
        }

        out
    }
}

pub struct AndroidAvds;

impl Provider for AndroidAvds {
    fn id(&self) -> &'static str {
        "android.avd"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let avd_home = ctx.paths.android_avd();
        if !avd_home.is_dir() {
            return Vec::new();
        }

        subdirs(&avd_home)
            .into_iter()
            .filter(|d| d.extension().is_some_and(|e| e == "avd"))
            .map(|dir| {
                let name = dir
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                // The .ini beside the .avd directory is the device registration;
                // leaving it behind gives a broken entry in the AVD manager.
                let ini = avd_home.join(format!("{name}.ini"));

                let mut builder = CandidateBuilder::new(
                    "android.avd",
                    Group::Android,
                    format!("emulator: {name}"),
                )
                .path(dir)
                .detail("A virtual device, including its full disk image and everything installed on it.")
                .tier(Tier::Caution)
                .regen(Regen::Rebuild { minutes: 5 })
                .warn(Warning::danger(
                    "Deletes the emulator's disk image: installed apps, accounts and app data inside it are lost.",
                ));

                if ini.is_file() {
                    builder = builder.path(ini);
                }
                builder.build()
            })
            .collect()
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(AndroidSdk), Box::new(AndroidAvds)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    use reclaim_core::platform::Base;

    /// A sandbox with its SDK and AVD directories redirected inside it.
    fn android_home() -> TestHome {
        let home = TestHome::new();
        home.redirect(Base::AndroidSdk, "sdk")
            .redirect(Base::AndroidAvd, "avd");
        home
    }

    #[test]
    fn system_images_are_offered_per_api_level() {
        let home = android_home();
        home.file(
            "sdk/system-images/android-34/google_apis/arm64/system.img",
            8192,
        );
        home.file(
            "sdk/system-images/android-33/google_apis/arm64/system.img",
            8192,
        );
        home.dir("avd");

        let found = home.discover(&AndroidSdk);

        let images: Vec<_> = found
            .iter()
            .filter(|c| c.provider == "android.system-image")
            .collect();
        assert_eq!(images.len(), 2, "one candidate per API level");
        assert!(images.iter().all(|i| i.tier == Tier::Review));
    }

    #[test]
    fn avds_are_caution_and_include_their_ini_file() {
        // Leaving the .ini behind produces a broken entry in the AVD manager.
        let home = android_home();
        home.file("avd/Pixel_7.avd/userdata-qemu.img", 16384);
        home.file("avd/Pixel_7.ini", 128);

        let found = home.discover(&AndroidAvds);

        assert_eq!(found.len(), 1);
        let avd = &found[0];
        assert_eq!(avd.tier, Tier::Caution);
        assert_eq!(
            avd.paths.len(),
            2,
            "the .avd dir and its .ini must go together"
        );
        assert!(avd.paths.iter().any(|p| p.ends_with("Pixel_7.ini")));
        assert!(avd
            .warnings
            .iter()
            .any(|w| w.severity == reclaim_core::Severity::Danger));
    }

    #[test]
    fn an_avd_without_an_ini_still_works() {
        let home = android_home();
        home.file("avd/Orphan.avd/userdata.img", 1024);

        let found = home.discover(&AndroidAvds);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].paths.len(), 1);
    }

    #[test]
    fn a_single_ndk_version_is_not_offered() {
        let home = android_home();
        home.dir("sdk/ndk/26.1.10909125");
        home.dir("avd");

        let found = home.discover(&AndroidSdk);
        assert!(!found.iter().any(|c| c.provider == "android.ndk"));
    }

    #[test]
    fn nothing_is_offered_without_an_android_sdk() {
        let home = android_home();
        assert!(home.discover(&AndroidSdk).is_empty());
    }
}
