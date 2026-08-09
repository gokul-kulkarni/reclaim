//! Rust: the cargo home caches and project `target/` directories.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

pub struct RustCaches;

impl Provider for RustCaches {
    fn id(&self) -> &'static str {
        "rust.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let cargo = ctx.paths.cargo_home();
        let mut out = Vec::new();

        // `registry/cache` holds the downloaded .crate archives; `registry/src` is
        // just those archives unpacked. Offering src separately is worthwhile
        // because it is usually the larger of the two and costs nothing to rebuild.
        if let Some(c) = global_cache(
            ctx,
            "rust.registry-src",
            Group::Rust,
            "cargo registry sources",
            cargo.join("registry/src"),
        ) {
            out.push(
                c.detail("Unpacked crate sources. Re-extracted from the local .crate archives.")
                    .regen(automatic("the next `cargo build`"))
                    .build(),
            );
        }

        if let Some(c) = global_cache(
            ctx,
            "rust.registry-cache",
            Group::Rust,
            "cargo registry archives",
            cargo.join("registry/cache"),
        ) {
            out.push(
                c.detail("Downloaded .crate archives.")
                    .tier(Tier::Review)
                    .regen(redownload("crates.io"))
                    .warn(Warning::info(
                        "Removing this forces a re-download; offline builds will fail until then.",
                    ))
                    .build(),
            );
        }

        if let Some(c) = global_cache(
            ctx,
            "rust.git-checkouts",
            Group::Rust,
            "cargo git checkouts",
            cargo.join("git"),
        ) {
            out.push(
                c.detail("Clones of git dependencies.")
                    .tier(Tier::Review)
                    .regen(redownload("the upstream repositories"))
                    .build(),
            );
        }

        for (id, label, path) in [
            (
                "rust.sccache",
                "sccache",
                ctx.paths.cache_dir().join("sccache"),
            ),
            (
                "rust.rustup-downloads",
                "rustup downloads",
                ctx.paths.home_join(".rustup/downloads"),
            ),
            (
                "rust.rustup-tmp",
                "rustup temp files",
                ctx.paths.home_join(".rustup/tmp"),
            ),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Rust, label, path) {
                out.push(c.regen(automatic("the next build")).build());
            }
        }

        // Old toolchains are large and easy to forget about, but removing the one
        // a project pins breaks that project until it is reinstalled.
        let toolchains = ctx.paths.home_join(".rustup/toolchains");
        if toolchains.is_dir() {
            let installed = subdirs(&toolchains);
            if installed.len() > 1 {
                for toolchain in installed {
                    let name = toolchain
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    out.push(
                        CandidateBuilder::new(
                            "rust.toolchain",
                            Group::Rust,
                            format!("rust toolchain: {name}"),
                        )
                        .path(toolchain)
                        .detail("An installed rustup toolchain.")
                        .tier(Tier::Review)
                        .regen(redownload("static.rust-lang.org"))
                        .warn(Warning::caution(format!(
                            "Any project pinning `{name}` in rust-toolchain.toml will not build \
                             until you reinstall it."
                        )))
                        .build(),
                    );
                }
            }
        }

        out
    }
}

pub struct RustTargets;

impl Provider for RustTargets {
    fn id(&self) -> &'static str {
        "rust.target"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["Cargo.toml"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        artifacts_in_projects(ctx, &["Cargo.toml"], |project| {
            let name = project
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            project_artifact(
                "rust.target",
                Group::Rust,
                "target/",
                &project.path,
                project.path.join("target"),
            )
            .map(|c| {
                c.detail(format!("Build output for `{name}`. Often the single largest directory in a Rust project."))
                    .regen(Regen::Rebuild { minutes: 4 })
                    .warn(Warning::info(
                        "Fully rebuilt by the next `cargo build`, but that build will be a cold one.",
                    ))
                    .build()
            })
            .into_iter()
            .collect()
        })
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(RustCaches), Box::new(RustTargets)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    fn registry_sources_are_safe_but_archives_need_review() {
        // src/ is re-extracted locally; cache/ requires the network.
        let home = TestHome::new();
        home.file(".cargo/registry/src/index/serde-1.0/lib.rs", 2048);
        home.file(".cargo/registry/cache/index/serde-1.0.crate", 4096);

        let found = home.discover(&RustCaches);
        let src = found
            .iter()
            .find(|c| c.provider == "rust.registry-src")
            .expect("src");
        let cache = found
            .iter()
            .find(|c| c.provider == "rust.registry-cache")
            .expect("cache");
        assert_eq!(src.tier, Tier::Safe);
        assert_eq!(cache.tier, Tier::Review);
    }

    #[test]
    fn target_directories_are_found_per_project() {
        let home = TestHome::new();
        home.project("dev/tool", &["Cargo.toml"]);
        home.file("dev/tool/target/debug/tool", 100_000);

        let found = home.discover(&RustTargets);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].project.as_deref(),
            Some(home.path("dev/tool").as_path())
        );
        assert!(matches!(found[0].regen, Regen::Rebuild { .. }));
    }

    #[test]
    fn a_project_without_a_target_dir_offers_nothing() {
        let home = TestHome::new();
        home.project("dev/tool", &["Cargo.toml"]);
        assert!(home.discover(&RustTargets).is_empty());
    }

    #[test]
    fn a_single_toolchain_is_never_offered() {
        // Removing the only toolchain leaves the machine unable to build anything.
        let home = TestHome::new();
        home.dir(".rustup/toolchains/stable-aarch64-apple-darwin");
        let found = home.discover(&RustCaches);
        assert!(!found.iter().any(|c| c.provider == "rust.toolchain"));
    }

    #[test]
    fn extra_toolchains_are_offered_with_a_pinning_warning() {
        let home = TestHome::new();
        home.dir(".rustup/toolchains/stable-aarch64-apple-darwin");
        home.dir(".rustup/toolchains/1.70.0-aarch64-apple-darwin");

        let found = home.discover(&RustCaches);
        let toolchains: Vec<_> = found
            .iter()
            .filter(|c| c.provider == "rust.toolchain")
            .collect();
        assert_eq!(toolchains.len(), 2);
        assert!(toolchains.iter().all(|t| t.tier == Tier::Review));
        assert!(toolchains.iter().all(|t| t
            .warnings
            .iter()
            .any(|w| w.message.contains("rust-toolchain.toml"))));
    }

    #[test]
    fn nothing_is_offered_without_rust_installed() {
        let home = TestHome::new();
        assert!(home.discover(&RustCaches).is_empty());
    }
}
