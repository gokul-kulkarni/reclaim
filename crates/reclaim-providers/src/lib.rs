//! Ecosystem providers: what each toolchain leaves on disk, how safe it is to
//! remove, and what it costs to get back.
//!
//! A provider is deliberately dumb and declarative. It does path-existence checks
//! and matches against the project list from the single stage-1 walk, then returns
//! candidates carrying a tier, a regeneration cost and any warnings a user needs
//! before deciding. It never measures, never walks, and never deletes.
//!
//! Adding an ecosystem means adding one file here and one line in [`all`].

pub mod android;
pub mod apple;
pub mod containers;
pub mod go;
pub mod jvm;
pub mod misc;
pub mod node;
pub mod python;
pub mod rust;
pub mod support;

/// Synthetic home directories for provider tests. Enabled by the `testing`
/// feature, or automatically inside this crate's own test build.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use reclaim_core::pipeline::Provider;

/// Every provider, in the order their groups are listed to the user.
pub fn all() -> Vec<Box<dyn Provider>> {
    let mut providers = Vec::new();
    providers.extend(node::providers());
    providers.extend(python::providers());
    providers.extend(rust::providers());
    providers.extend(jvm::providers());
    providers.extend(go::providers());
    providers.extend(apple::providers());
    providers.extend(android::providers());
    providers.extend(containers::providers());
    providers.extend(misc::providers());
    providers
}

/// Every project marker file any provider cares about, for the stage-1 walk.
pub fn all_markers() -> Vec<&'static str> {
    let mut markers: Vec<&'static str> = all().iter().flat_map(|p| p.markers().to_vec()).collect();
    markers.sort_unstable();
    markers.dedup();
    markers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;
    use std::collections::HashSet;

    #[test]
    fn every_provider_has_a_unique_id() {
        let providers = all();
        let ids: HashSet<&str> = providers.iter().map(|p| p.id()).collect();
        assert_eq!(ids.len(), providers.len(), "duplicate provider ids");
    }

    #[test]
    fn every_provider_id_is_dotted_so_group_filtering_works() {
        // `reclaim clean --provider node` relies on the `<group>.<name>` shape.
        for provider in all() {
            let id = provider.id();
            assert!(id.contains('.'), "`{id}` must be `<group>.<name>`");
            assert!(
                !id.starts_with('.') && !id.ends_with('.'),
                "`{id}` is malformed"
            );
        }
    }

    #[test]
    fn an_empty_home_produces_no_candidates_from_path_based_providers() {
        // Nothing may be invented on a machine that has none of these toolchains.
        let home = TestHome::new();
        let path_based: Vec<_> = all()
            .into_iter()
            .filter(|p| {
                !matches!(
                    p.id(),
                    "system.caches" | "containers.docker" | "apple.simulators"
                )
            })
            .collect();

        let found = home.discover_all(&path_based);
        assert!(found.is_empty(), "unexpected candidates: {found:#?}");
    }

    #[test]
    fn every_discovered_candidate_carries_at_least_one_real_path() {
        let home = TestHome::new();
        home.file(".npm/_cacache/index", 1024);
        home.file(".gradle/caches/x.jar", 1024);
        home.project("dev/app", &["package.json", "package-lock.json"]);
        home.file("dev/app/node_modules/react/index.js", 1024);

        for candidate in home.discover_all(&all()) {
            assert!(
                !candidate.paths.is_empty(),
                "{} has no paths",
                candidate.provider
            );
            assert!(
                candidate.paths.iter().all(|p| p.is_absolute()),
                "{} has a relative path",
                candidate.provider
            );
        }
    }

    #[test]
    fn every_candidate_has_a_label_and_a_regeneration_story() {
        let home = TestHome::new();
        home.file(".npm/_cacache/index", 1024);
        home.file(".cargo/registry/src/x/lib.rs", 1024);
        home.file(".gradle/caches/x.jar", 1024);

        for candidate in home.discover_all(&all()) {
            assert!(
                !candidate.label.is_empty(),
                "{} has no label",
                candidate.provider
            );
            assert!(
                !candidate.regen.summary().is_empty(),
                "{} does not say how it comes back",
                candidate.provider
            );
        }
    }

    #[test]
    fn irreplaceable_candidates_always_carry_a_warning() {
        // Tier::Caution without an explanation is exactly the failure mode this
        // whole tool exists to avoid.
        let home = TestHome::new();
        home.project("dev/app", &["package.json"]); // no lockfile -> caution
        home.file("dev/app/node_modules/react/index.js", 1024);
        home.project("dev/site", &["composer.json"]);
        home.file("dev/site/vendor/autoload.php", 1024);

        for candidate in home.discover_all(&all()) {
            if candidate.tier == reclaim_core::Tier::Caution {
                assert!(
                    !candidate.warnings.is_empty(),
                    "{} is Caution tier but explains nothing",
                    candidate.provider
                );
            }
        }
    }

    #[test]
    fn markers_are_collected_and_deduplicated() {
        let markers = all_markers();
        assert!(markers.contains(&"package.json"));
        assert!(markers.contains(&"Cargo.toml"));
        assert!(markers.contains(&"pom.xml"));
        let unique: HashSet<_> = markers.iter().collect();
        assert_eq!(unique.len(), markers.len());
    }

    #[test]
    fn the_provider_set_covers_every_group_we_advertise() {
        let home = TestHome::new();
        let ids: HashSet<&str> = all()
            .iter()
            .map(|p| p.id().split('.').next().unwrap())
            .collect();
        for expected in [
            "node",
            "python",
            "rust",
            "jvm",
            "go",
            "apple",
            "android",
            "containers",
            "dotnet",
            "ruby",
            "php",
            "dart",
            "buildtools",
            "editors",
            "system",
            "ml",
        ] {
            assert!(ids.contains(expected), "no provider for `{expected}`");
        }
        drop(home);
    }
}
