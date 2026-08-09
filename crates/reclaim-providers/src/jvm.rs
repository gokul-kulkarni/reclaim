//! JVM: Maven, Gradle, sbt, Ivy and their project build directories.

use std::path::Path;

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

const MARKERS: &[&str] = &[
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "build.sbt",
];

/// Cap on how many Maven version directories we inspect looking for locally
/// installed artifacts. Discovery must stay cheap; a bounded sample is enough to
/// answer "does this repository contain anything that is not re-downloadable?".
const MAVEN_SCAN_BUDGET: usize = 4000;

pub struct JvmCaches;

impl Provider for JvmCaches {
    fn id(&self) -> &'static str {
        "jvm.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        // Gradle's caches are the classic easy win: large, and fully regenerated.
        for (id, label, rel, detail) in [
            (
                "jvm.gradle-caches",
                "Gradle caches",
                ".gradle/caches",
                "Downloaded dependencies and build caches.",
            ),
            (
                "jvm.gradle-daemon",
                "Gradle daemon logs",
                ".gradle/daemon",
                "Daemon logs and registry files.",
            ),
            (
                "jvm.gradle-native",
                "Gradle native binaries",
                ".gradle/native",
                "Platform helper binaries.",
            ),
            (
                "jvm.gradle-wrapper",
                "Gradle wrapper distributions",
                ".gradle/wrapper/dists",
                "Downloaded Gradle distributions, one per version any project has used.",
            ),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Jvm, label, p.home_join(rel)) {
                out.push(
                    c.detail(detail)
                        .regen(redownload("Maven Central and services.gradle.org"))
                        .build(),
                );
            }
        }

        // Maven is the one that needs care: `mvn install` puts artifacts here that
        // exist in no remote repository, and nothing distinguishes them at a glance.
        let m2 = p.home_join(".m2/repository");
        if m2.is_dir() {
            let local_only = count_locally_installed(&m2, MAVEN_SCAN_BUDGET);
            out.push(
                CandidateBuilder::new("jvm.maven-repo", Group::Jvm, "Maven local repository")
                    .path(m2)
                    .detail("Every dependency Maven has ever downloaded, plus anything you have `mvn install`ed.")
                    .tier(if local_only.found > 0 { Tier::Caution } else { Tier::Review })
                    .regen(if local_only.found > 0 { Regen::Never } else { redownload("Maven Central") })
                    .warn_if(
                        local_only.found > 0,
                        Warning::danger(format!(
                            "Found {}{} artifact(s) with no `_remote.repositories` marker: these came \
                             from `mvn install` and are not re-downloadable from any repository.",
                            local_only.found,
                            if local_only.truncated { "+" } else { "" }
                        )),
                    )
                    .warn_if(
                        local_only.found == 0,
                        Warning::info("Everything here appears to be re-downloadable from a remote repository."),
                    )
                    .build(),
            );
        }

        for (id, label, rel) in [
            ("jvm.ivy", "Ivy cache", ".ivy2/cache"),
            ("jvm.sbt", "sbt boot and cache", ".sbt/boot"),
            ("jvm.coursier", "Coursier cache", ".cache/coursier"),
            ("jvm.konan", "Kotlin/Native dependencies", ".konan"),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Jvm, label, p.home_join(rel)) {
                out.push(
                    c.tier(Tier::Review)
                        .regen(redownload("the configured repositories"))
                        .build(),
                );
            }
        }

        if let Some(c) = global_cache(
            ctx,
            "jvm.coursier",
            Group::Jvm,
            "Coursier cache",
            p.cache_dir().join("Coursier"),
        ) {
            out.push(
                c.tier(Tier::Review)
                    .regen(redownload("the configured repositories"))
                    .build(),
            );
        }

        out
    }
}

pub struct JvmProjects;

impl Provider for JvmProjects {
    fn id(&self) -> &'static str {
        "jvm.projects"
    }

    fn markers(&self) -> &'static [&'static str] {
        MARKERS
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        artifacts_in_projects(ctx, MARKERS, |project| {
            let mut out = Vec::new();

            for (dir, label) in [
                ("build", "Gradle build output"),
                ("target", "Maven build output"),
                ("out", "IDE build output"),
            ] {
                // Only claim `target/` for a Maven project, and `build/` for Gradle:
                // both names are used by other ecosystems and by hand-written scripts.
                let is_maven = project.has_marker("pom.xml");
                let is_gradle = project.has_any_marker(&[
                    "build.gradle",
                    "build.gradle.kts",
                    "settings.gradle",
                    "settings.gradle.kts",
                ]);
                let claim = match dir {
                    "target" => is_maven,
                    "build" => is_gradle,
                    _ => is_maven || is_gradle,
                };
                if !claim {
                    continue;
                }

                if let Some(c) = project_artifact(
                    "jvm.build-output",
                    Group::Jvm,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(Regen::Rebuild { minutes: 3 }).build());
                }
            }

            // Per-project Gradle state, safe and often surprisingly large.
            if let Some(c) = project_artifact(
                "jvm.project-gradle",
                Group::Jvm,
                "project .gradle cache",
                &project.path,
                project.path.join(".gradle"),
            ) {
                out.push(c.regen(automatic("the next Gradle build")).build());
            }

            out
        })
    }
}

/// Result of the bounded scan for locally installed Maven artifacts.
#[derive(Debug, Default, PartialEq)]
struct LocalArtifacts {
    found: usize,
    /// The budget ran out, so `found` is a floor rather than an exact count.
    truncated: bool,
}

/// Count version directories that hold a jar but no `_remote.repositories`.
///
/// Maven writes `_remote.repositories` next to anything it downloaded. A jar
/// without one was produced by `mvn install` on this machine and exists nowhere
/// else, so the whole repository has to be treated as irreplaceable.
fn count_locally_installed(repo: &Path, budget: usize) -> LocalArtifacts {
    let mut result = LocalArtifacts::default();
    let mut examined = 0usize;

    let walker = jwalk_lite(repo);
    for dir in walker {
        if examined >= budget {
            result.truncated = true;
            break;
        }
        examined += 1;

        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut has_jar = false;
        let mut has_remote_marker = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "_remote.repositories" {
                has_remote_marker = true;
            } else if name.ends_with(".jar") || name.ends_with(".aar") {
                has_jar = true;
            }
        }
        if has_jar && !has_remote_marker {
            result.found += 1;
            // One is enough to change the verdict; keep counting only a little
            // further so the message can say "several" honestly without a full walk.
            if result.found >= 25 {
                result.truncated = true;
                break;
            }
        }
    }

    result
}

/// Depth-limited directory iterator: Maven lays out `group/artifact/version`, so
/// artifacts live 3-6 levels down and there is no reason to go deeper.
fn jwalk_lite(root: &Path) -> Vec<std::path::PathBuf> {
    fn recurse(dir: &Path, depth: usize, max: usize, out: &mut Vec<std::path::PathBuf>) {
        if depth > max || out.len() > 20_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let path = entry.path();
                out.push(path.clone());
                recurse(&path, depth + 1, max, out);
            }
        }
    }
    let mut out = Vec::new();
    recurse(root, 0, 6, &mut out);
    out
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(JvmCaches), Box::new(JvmProjects)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    fn gradle_caches_are_safe() {
        let home = TestHome::new();
        home.file(".gradle/caches/modules-2/files/x.jar", 8192);
        let found = home.discover(&JvmCaches);
        let caches = found
            .iter()
            .find(|c| c.provider == "jvm.gradle-caches")
            .expect("gradle");
        assert_eq!(caches.tier, Tier::Safe);
    }

    #[test]
    fn a_purely_downloaded_maven_repo_is_review_tier() {
        let home = TestHome::new();
        home.file(".m2/repository/com/example/lib/1.0/lib-1.0.jar", 4096);
        home.file(
            ".m2/repository/com/example/lib/1.0/_remote.repositories",
            64,
        );

        let found = home.discover(&JvmCaches);
        let m2 = found
            .iter()
            .find(|c| c.provider == "jvm.maven-repo")
            .expect("m2");
        assert_eq!(m2.tier, Tier::Review);
        assert!(matches!(m2.regen, Regen::Download { .. }));
    }

    #[test]
    fn a_maven_repo_with_locally_installed_artifacts_is_caution_and_unrecoverable() {
        // This is the case that a plain `rm -rf ~/.m2` silently destroys.
        let home = TestHome::new();
        home.file(".m2/repository/com/example/lib/1.0/lib-1.0.jar", 4096);
        home.file(
            ".m2/repository/com/example/lib/1.0/_remote.repositories",
            64,
        );
        home.file(
            ".m2/repository/com/mine/internal/2.0/internal-2.0.jar",
            4096,
        );

        let found = home.discover(&JvmCaches);
        let m2 = found
            .iter()
            .find(|c| c.provider == "jvm.maven-repo")
            .expect("m2");
        assert_eq!(m2.tier, Tier::Caution);
        assert_eq!(m2.regen, Regen::Never);
        let warning = m2
            .warnings
            .iter()
            .find(|w| w.message.contains("_remote.repositories"))
            .expect("must explain why");
        assert!(warning.message.contains("mvn install"));
    }

    #[test]
    fn maven_target_is_not_claimed_for_a_gradle_project() {
        // `target/` in a Gradle project is not Maven output and may be anything.
        let home = TestHome::new();
        home.project("dev/app", &["build.gradle"]);
        home.file("dev/app/target/stuff.bin", 1024);
        home.file("dev/app/build/libs/app.jar", 2048);

        let found = home.discover(&JvmProjects);
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"Gradle build output"));
        assert!(!labels.contains(&"Maven build output"));
    }

    #[test]
    fn gradle_build_is_not_claimed_for_a_maven_project() {
        let home = TestHome::new();
        home.project("dev/app", &["pom.xml"]);
        home.file("dev/app/target/classes/A.class", 512);
        home.file("dev/app/build/notes.txt", 32);

        let found = home.discover(&JvmProjects);
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"Maven build output"));
        assert!(!labels.contains(&"Gradle build output"));
    }

    #[test]
    fn local_artifact_detection_reports_zero_for_a_clean_repo() {
        let home = TestHome::new();
        home.file(".m2/repository/a/b/1.0/b-1.0.jar", 10);
        home.file(".m2/repository/a/b/1.0/_remote.repositories", 10);
        let result = count_locally_installed(&home.path(".m2/repository"), 1000);
        assert_eq!(
            result,
            LocalArtifacts {
                found: 0,
                truncated: false
            }
        );
    }

    #[test]
    fn nothing_is_offered_without_a_jvm_toolchain() {
        let home = TestHome::new();
        assert!(home.discover(&JvmCaches).is_empty());
    }
}
