//! Go: the module cache and the build cache.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

pub struct GoCaches;

impl Provider for GoCaches {
    fn id(&self) -> &'static str {
        "go.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["go.mod"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let mut out = Vec::new();

        // The build cache is pure derived data and the safest large item Go has.
        if let Some(c) = global_cache(
            ctx,
            "go.build-cache",
            Group::Go,
            "Go build cache",
            ctx.paths.go_build_cache(),
        ) {
            out.push(
                c.detail("Compiled package objects, keyed by content hash.")
                    .regen(automatic("the next `go build`"))
                    .build(),
            );
        }

        // The module cache is written read-only, which is why plain `rm -rf` fails
        // on it and `go clean -modcache` exists. Offer the command, not the path.
        let modcache = ctx.paths.go_mod_cache();
        if modcache.is_dir() {
            let mut builder = CandidateBuilder::new("go.mod-cache", Group::Go, "Go module cache")
                .path(modcache)
                .detail("Downloaded module sources. Written read-only by the Go toolchain.")
                .tier(Tier::Review)
                .regen(redownload("the Go module proxy"))
                .warn(Warning::info(
                    "Go marks these files read-only, so this is cleaned with `go clean -modcache` \
                     rather than a plain delete.",
                ));

            if has_command("go") {
                builder = builder.action(reclaim_core::model::Action::Shell {
                    program: "go".into(),
                    args: vec!["clean".into(), "-modcache".into()],
                });
            } else {
                builder = builder.warn(Warning::caution(
                    "`go` is not on PATH, so this will be removed with a forced delete instead.",
                ));
            }

            out.push(builder.build());
        }

        out
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(GoCaches)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    use reclaim_core::platform::Base;

    /// A sandbox with GOPATH and GOCACHE redirected inside it.
    fn go_home() -> TestHome {
        let home = TestHome::new();
        home.redirect(Base::GoPath, "go")
            .redirect(Base::GoBuildCache, "go-cache");
        home
    }

    #[test]
    fn the_build_cache_is_safe() {
        let home = go_home();
        home.file("go-cache/ab/abcdef", 4096);
        home.dir("go");

        let found = home.discover(&GoCaches);
        let build = found
            .iter()
            .find(|c| c.provider == "go.build-cache")
            .expect("build cache");
        assert_eq!(build.tier, Tier::Safe);
    }

    #[test]
    fn the_module_cache_uses_go_clean_because_its_files_are_read_only() {
        // A plain rm -rf fails partway through and leaves a broken cache.
        let home = go_home();
        home.file("go/pkg/mod/github.com/x/y@v1.0.0/go.mod", 512);
        home.dir("go-cache");

        let found = home.discover(&GoCaches);
        let modcache = found
            .iter()
            .find(|c| c.provider == "go.mod-cache")
            .expect("mod cache");
        assert_eq!(modcache.tier, Tier::Review);
        assert!(modcache
            .warnings
            .iter()
            .any(|w| w.message.contains("read-only")));

        if has_command("go") {
            match &modcache.action {
                reclaim_core::Action::Shell { program, args } => {
                    assert_eq!(program, "go");
                    assert_eq!(args, &["clean", "-modcache"]);
                }
                other => panic!("expected a shell action, got {other:?}"),
            }
        }
    }

    #[test]
    fn nothing_is_offered_without_go() {
        let home = go_home();
        assert!(home.discover(&GoCaches).is_empty());
    }
}
