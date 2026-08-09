//! Node.js: package manager caches, browser downloads and `node_modules`.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

/// Files that pin exact dependency versions. Without one of these, reinstalling
/// `node_modules` may not reproduce what is there now.
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "npm-shrinkwrap.json",
];

pub struct NodeCaches;

impl Provider for NodeCaches {
    fn id(&self) -> &'static str {
        "node.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        if let Some(c) = global_cache(
            ctx,
            "node.npm-cache",
            Group::Node,
            "npm cache",
            p.home_join(".npm"),
        ) {
            out.push(
                c.detail("Downloaded tarballs and metadata under ~/.npm/_cacache.")
                    .regen(automatic("the next `npm install`"))
                    .build(),
            );
        }

        // The pnpm store is the one genuine footgun in this ecosystem: it is
        // content-addressed and hardlinked into every node_modules on the machine.
        for store in [
            p.home_join("Library/pnpm/store"),
            p.home_join(".pnpm-store"),
            p.home_join(".local/share/pnpm/store"),
        ] {
            if !store.exists() {
                continue;
            }
            if ctx.config.providers.node.keep_pnpm_store {
                continue;
            }
            out.push(
                CandidateBuilder::new(
                    "node.pnpm-store",
                    Group::Node,
                    "pnpm content-addressable store",
                )
                .path(store)
                .detail(
                    "pnpm stores each package version once and hardlinks it into every \
                         project. The bytes shown as `shared` are already counted elsewhere.",
                )
                .tier(Tier::Review)
                .regen(redownload("the npm registry"))
                .warn(hardlink_store_warning("pnpm"))
                .build(),
            );
        }

        for (label, path) in [
            ("Yarn v1 cache", p.cache_dir().join("Yarn")),
            ("Yarn Berry cache", p.home_join(".yarn/berry/cache")),
            ("Yarn global cache", p.home_join(".cache/yarn")),
        ] {
            if let Some(c) = global_cache(ctx, "node.yarn-cache", Group::Node, label, path) {
                out.push(c.regen(automatic("the next `yarn install`")).build());
            }
        }

        if let Some(c) = global_cache(
            ctx,
            "node.bun-cache",
            Group::Node,
            "Bun install cache",
            p.home_join(".bun/install/cache"),
        ) {
            out.push(c.regen(automatic("the next `bun install`")).build());
        }

        if let Some(c) = global_cache(
            ctx,
            "node.node-gyp",
            Group::Node,
            "node-gyp headers",
            p.home_join(".node-gyp"),
        ) {
            out.push(
                c.detail("Node header archives used to compile native addons.")
                    .regen(redownload("nodejs.org"))
                    .build(),
            );
        }

        // Browser binaries: individually large, and re-downloading them is slow
        // enough that the user should know it is a download rather than a rebuild.
        for (id, label, path, source) in [
            (
                "node.playwright",
                "Playwright browsers",
                p.cache_dir().join("ms-playwright"),
                "Playwright CDN",
            ),
            (
                "node.puppeteer",
                "Puppeteer browsers",
                p.cache_dir().join("puppeteer"),
                "the Chromium CDN",
            ),
            (
                "node.cypress",
                "Cypress binaries",
                p.cache_dir().join("Cypress"),
                "the Cypress CDN",
            ),
            (
                "node.electron",
                "Electron binaries",
                p.cache_dir().join("electron"),
                "the Electron CDN",
            ),
        ] {
            if !path.exists() {
                continue;
            }
            out.push(
                CandidateBuilder::new(id, Group::Node, label)
                    .path(path)
                    .tier(Tier::Review)
                    .regen(redownload(source))
                    .warn(Warning::info(
                        "Re-downloaded automatically, but it is a large download on a slow link.",
                    ))
                    .build(),
            );
        }

        out
    }
}

pub struct NodeModules;

impl Provider for NodeModules {
    fn id(&self) -> &'static str {
        "node.modules"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["package.json"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        if !ctx.config.providers.node.offer_node_modules {
            return Vec::new();
        }

        artifacts_in_projects(ctx, &["package.json"], |project| {
            let mut out = Vec::new();
            let has_lockfile = any_file_exists(&project.path, LOCKFILES);

            if let Some(c) = project_artifact(
                "node.modules",
                Group::Node,
                "node_modules",
                &project.path,
                project.path.join("node_modules"),
            ) {
                let name = project
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                out.push(
                    c.detail(format!("Installed dependencies for `{name}`."))
                        // Without a lockfile a reinstall is a gamble, not a formality.
                        .tier(if has_lockfile {
                            Tier::Safe
                        } else {
                            Tier::Caution
                        })
                        .regen(if has_lockfile {
                            redownload("the npm registry")
                        } else {
                            reclaim_core::model::Regen::Never
                        })
                        .warn_if(!has_lockfile, no_lockfile_warning("npm install"))
                        .build(),
                );
            }

            // Framework build output. Always regenerated, never precious.
            for (dir, label) in [
                (".next", "Next.js build output"),
                (".nuxt", "Nuxt build output"),
                (".turbo", "Turborepo cache"),
                (".parcel-cache", "Parcel cache"),
                (".svelte-kit", "SvelteKit build output"),
                (".angular", "Angular cache"),
            ] {
                if let Some(c) = project_artifact(
                    "node.build-output",
                    Group::Node,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(automatic("the next build")).build());
                }
            }

            out
        })
    }
}

/// Both Node providers.
pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(NodeCaches), Box::new(NodeModules)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;
    use reclaim_core::model::Regen;

    #[test]
    fn finds_the_npm_cache() {
        let home = TestHome::new();
        home.dir(".npm/_cacache").file(".npm/_cacache/index", 4096);

        let found = home.discover(&NodeCaches);
        let npm = found
            .iter()
            .find(|c| c.provider == "node.npm-cache")
            .expect("npm cache");
        assert_eq!(npm.tier, Tier::Safe);
        assert!(matches!(npm.regen, Regen::Automatic { .. }));
    }

    #[test]
    fn no_candidates_on_a_machine_without_node() {
        let home = TestHome::new();
        assert!(home.discover(&NodeCaches).is_empty());
    }

    #[test]
    fn the_pnpm_store_is_withheld_by_default() {
        // Deleting it breaks every existing install for a small real gain, so the
        // default config opts out entirely.
        let home = TestHome::new();
        home.dir("Library/pnpm/store/v3")
            .file("Library/pnpm/store/v3/blob", 1024);
        home.dir(".local/share/pnpm/store/v3");

        let found = home.discover(&NodeCaches);
        assert!(!found.iter().any(|c| c.provider == "node.pnpm-store"));
    }

    #[test]
    fn the_pnpm_store_warns_about_hardlinks_when_enabled() {
        let mut home = TestHome::new();
        home.config.providers.node.keep_pnpm_store = false;
        home.dir("Library/pnpm/store/v3")
            .file("Library/pnpm/store/v3/blob", 1024);

        let found = home.discover(&NodeCaches);
        let store = found
            .iter()
            .find(|c| c.provider == "node.pnpm-store")
            .expect("pnpm store");
        assert_eq!(store.tier, Tier::Review);
        assert!(
            store
                .warnings
                .iter()
                .any(|w| w.message.contains("hardlinked")),
            "the hardlink hazard must be stated: {:?}",
            store.warnings
        );
    }

    #[test]
    fn node_modules_with_a_lockfile_is_safe() {
        let home = TestHome::new();
        home.project("dev/app", &["package.json", "package-lock.json"]);
        home.dir("dev/app/node_modules/react")
            .file("dev/app/node_modules/react/index.js", 2048);

        let found = home.discover(&NodeModules);
        let modules = found
            .iter()
            .find(|c| c.provider == "node.modules")
            .expect("node_modules");
        assert_eq!(modules.tier, Tier::Safe);
        assert!(modules.warnings.is_empty());
        assert_eq!(
            modules.project.as_deref(),
            Some(home.path("dev/app").as_path())
        );
    }

    #[test]
    fn node_modules_without_a_lockfile_is_caution() {
        // A reinstall may silently resolve different versions, which is exactly the
        // kind of thing the user should be told before deleting.
        let home = TestHome::new();
        home.project("dev/app", &["package.json"]);
        home.dir("dev/app/node_modules/react")
            .file("dev/app/node_modules/react/index.js", 2048);

        let found = home.discover(&NodeModules);
        let modules = found
            .iter()
            .find(|c| c.provider == "node.modules")
            .expect("node_modules");
        assert_eq!(modules.tier, Tier::Caution);
        assert_eq!(modules.regen, Regen::Never);
        assert!(modules
            .warnings
            .iter()
            .any(|w| w.message.contains("lockfile")));
    }

    #[test]
    fn framework_build_output_is_found() {
        let home = TestHome::new();
        home.project("dev/site", &["package.json", "package-lock.json"]);
        home.dir("dev/site/.next")
            .file("dev/site/.next/build.js", 1024);

        let found = home.discover(&NodeModules);
        assert!(found.iter().any(|c| c.label.contains("Next.js")));
    }

    #[test]
    fn node_modules_can_be_disabled_in_config() {
        let mut home = TestHome::new();
        home.config.providers.node.offer_node_modules = false;
        home.project("dev/app", &["package.json"]);
        home.dir("dev/app/node_modules");

        assert!(home.discover(&NodeModules).is_empty());
    }

    #[test]
    fn browser_caches_are_flagged_as_a_download() {
        let home = TestHome::new();
        home.dir("Library/Caches/ms-playwright/chromium-1234");

        let found = home.discover(&NodeCaches);
        let pw = found.iter().find(|c| c.provider == "node.playwright");
        // Only present on macOS layouts; on Linux the cache dir differs.
        if let Some(pw) = pw {
            assert_eq!(pw.tier, Tier::Review);
            assert!(matches!(pw.regen, Regen::Download { .. }));
        }
    }
}
