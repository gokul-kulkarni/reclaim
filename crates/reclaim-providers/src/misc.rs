//! The remaining ecosystems, each small enough that a shared file is clearer
//! than ten near-identical ones: .NET, Ruby, PHP, Dart/Flutter, build tools,
//! editors, system caches and ML model caches.

use reclaim_core::model::{Action, Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

// ---------------------------------------------------------------------------
// .NET
// ---------------------------------------------------------------------------

pub struct DotNet;

impl Provider for DotNet {
    fn id(&self) -> &'static str {
        "dotnet.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["Directory.Build.props"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        if let Some(c) = global_cache(
            ctx,
            "dotnet.nuget",
            Group::DotNet,
            "NuGet package cache",
            p.home_join(".nuget/packages"),
        ) {
            out.push(c.tier(Tier::Review).regen(redownload("nuget.org")).build());
        }
        if let Some(c) = global_cache(
            ctx,
            "dotnet.http-cache",
            Group::DotNet,
            "NuGet HTTP cache",
            p.home_join(".local/share/NuGet/http-cache"),
        ) {
            out.push(c.regen(redownload("nuget.org")).build());
        }

        for project in ctx.projects_with(&["Directory.Build.props", "*.csproj", "*.sln"]) {
            for (dir, label) in [("bin", ".NET bin output"), ("obj", ".NET obj output")] {
                if let Some(c) = project_artifact(
                    "dotnet.build-output",
                    Group::DotNet,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(Regen::Rebuild { minutes: 2 }).build());
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

pub struct Ruby;

impl Provider for Ruby {
    fn id(&self) -> &'static str {
        "ruby.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["Gemfile"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        // Only the downloaded .gem archives are cache; the unpacked gems under
        // `gems/` are the installation itself and must not be offered.
        for rel in [".gem/cache", ".gem/ruby"] {
            if let Some(c) = global_cache(
                ctx,
                "ruby.gem-cache",
                Group::Ruby,
                "RubyGems cache",
                p.home_join(rel),
            ) {
                out.push(
                    c.tier(Tier::Review)
                        .regen(redownload("rubygems.org"))
                        .build(),
                );
            }
        }
        if let Some(c) = global_cache(
            ctx,
            "ruby.bundler-cache",
            Group::Ruby,
            "Bundler cache",
            p.cache_dir().join("bundle"),
        ) {
            out.push(c.regen(redownload("rubygems.org")).build());
        }

        for project in ctx.projects_with(&["Gemfile"]) {
            let has_lock = project.path.join("Gemfile.lock").is_file();
            if let Some(c) = project_artifact(
                "ruby.vendor-bundle",
                Group::Ruby,
                "vendor/bundle",
                &project.path,
                project.path.join("vendor/bundle"),
            ) {
                out.push(
                    c.tier(if has_lock {
                        Tier::Review
                    } else {
                        Tier::Caution
                    })
                    .regen(if has_lock {
                        redownload("rubygems.org")
                    } else {
                        Regen::Never
                    })
                    .warn_if(!has_lock, no_lockfile_warning("bundle install"))
                    .build(),
                );
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

pub struct Php;

impl Provider for Php {
    fn id(&self) -> &'static str {
        "php.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["composer.json"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        for path in [
            p.cache_dir().join("composer"),
            p.home_join(".composer/cache"),
        ] {
            if let Some(c) = global_cache(
                ctx,
                "php.composer-cache",
                Group::Php,
                "Composer cache",
                path,
            ) {
                out.push(c.regen(redownload("Packagist")).build());
            }
        }

        for project in ctx.projects_with(&["composer.json"]) {
            let has_lock = project.path.join("composer.lock").is_file();
            if let Some(c) = project_artifact(
                "php.vendor",
                Group::Php,
                "vendor/",
                &project.path,
                project.path.join("vendor"),
            ) {
                out.push(
                    c.tier(if has_lock {
                        Tier::Review
                    } else {
                        Tier::Caution
                    })
                    .regen(if has_lock {
                        redownload("Packagist")
                    } else {
                        Regen::Never
                    })
                    .warn_if(!has_lock, no_lockfile_warning("composer install"))
                    .build(),
                );
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Dart / Flutter
// ---------------------------------------------------------------------------

pub struct Dart;

impl Provider for Dart {
    fn id(&self) -> &'static str {
        "dart.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["pubspec.yaml"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        if let Some(c) = global_cache(
            ctx,
            "dart.pub-cache",
            Group::Dart,
            "Pub package cache",
            p.home_join(".pub-cache"),
        ) {
            out.push(c.tier(Tier::Review).regen(redownload("pub.dev")).build());
        }

        // Flutter's downloaded engine binaries: several GB, and re-fetched by
        // `flutter precache`, which is slow but entirely automatic.
        for rel in ["flutter/bin/cache", "development/flutter/bin/cache"] {
            if let Some(c) = global_cache(
                ctx,
                "dart.flutter-engine",
                Group::Dart,
                "Flutter engine cache",
                p.home_join(rel),
            ) {
                out.push(
                    c.detail("Downloaded Flutter engine and tool binaries.")
                        .tier(Tier::Review)
                        .regen(redownload("the Flutter storage bucket"))
                        .warn(Warning::info(
                            "Restored by `flutter precache`, a multi-gigabyte download.",
                        ))
                        .build(),
                );
            }
        }

        for project in ctx.projects_with(&["pubspec.yaml"]) {
            for (dir, label) in [
                (".dart_tool", "Dart tool cache"),
                ("build", "Flutter build output"),
            ] {
                if let Some(c) = project_artifact(
                    "dart.build-output",
                    Group::Dart,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(Regen::Rebuild { minutes: 3 }).build());
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Build tools
// ---------------------------------------------------------------------------

pub struct BuildTools;

impl Provider for BuildTools {
    fn id(&self) -> &'static str {
        "buildtools.caches"
    }

    fn markers(&self) -> &'static [&'static str] {
        &["CMakeLists.txt", "WORKSPACE", "MODULE.bazel"]
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        for (id, label, path) in [
            ("buildtools.ccache", "ccache", p.cache_dir().join("ccache")),
            ("buildtools.ccache", "ccache", p.home_join(".ccache")),
            (
                "buildtools.bazel",
                "Bazel disk cache",
                p.cache_dir().join("bazel"),
            ),
            (
                "buildtools.bazelisk",
                "Bazelisk downloads",
                p.cache_dir().join("bazelisk"),
            ),
            ("buildtools.zig", "Zig cache", p.cache_dir().join("zig")),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::BuildTools, label, path) {
                out.push(c.regen(Regen::Rebuild { minutes: 5 }).build());
            }
        }

        for project in ctx.projects_with(&["CMakeLists.txt", "WORKSPACE", "MODULE.bazel"]) {
            // Only claim `build/` when CMake actually generated it; the name is
            // used by too many other things to claim on sight.
            let cmake_build = project.path.join("build");
            if cmake_build.join("CMakeCache.txt").is_file() {
                if let Some(c) = project_artifact(
                    "buildtools.cmake-build",
                    Group::BuildTools,
                    "CMake build directory",
                    &project.path,
                    cmake_build,
                ) {
                    out.push(c.regen(Regen::Rebuild { minutes: 6 }).build());
                }
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Editors and IDEs
// ---------------------------------------------------------------------------

pub struct Editors;

impl Provider for Editors {
    fn id(&self) -> &'static str {
        "editors.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        let app_support = if cfg!(target_os = "macos") {
            p.home_join("Library/Application Support")
        } else {
            p.home_join(".config")
        };

        for editor in ["Code", "Cursor", "VSCodium", "Code - Insiders", "Windsurf"] {
            let base = app_support.join(editor);
            if !base.is_dir() {
                continue;
            }

            for (sub, label, tier) in [
                ("Cache", "cache", Tier::Safe),
                ("CachedData", "cached data", Tier::Safe),
                (
                    "CachedExtensionVSIXs",
                    "cached extension packages",
                    Tier::Safe,
                ),
                ("logs", "logs", Tier::Safe),
                ("Crashpad", "crash reports", Tier::Safe),
            ] {
                let path = base.join(sub);
                if !path.exists() {
                    continue;
                }
                out.push(
                    CandidateBuilder::new(
                        "editors.vscode",
                        Group::Editors,
                        format!("{editor} {label}"),
                    )
                    .path(path)
                    .tier(tier)
                    .regen(automatic("normal use"))
                    .build(),
                );
            }

            // Workspace storage holds per-project editor state: open tabs, undo
            // history, extension state. Recoverable, but the user will notice.
            let workspace = base.join("User/workspaceStorage");
            if workspace.is_dir() {
                out.push(
                    CandidateBuilder::new(
                        "editors.workspace-storage",
                        Group::Editors,
                        format!("{editor} workspace storage"),
                    )
                    .path(workspace)
                    .detail("Per-project editor state: open editors, undo history, extension data.")
                    .tier(Tier::Review)
                    .regen(Regen::Never)
                    .warn(Warning::caution(
                        "Loses per-project editor state such as open tabs and local history.",
                    ))
                    .build(),
                );
            }
        }

        // JetBrains keeps caches and logs outside the IDE install.
        for (label, rel) in [
            ("JetBrains caches", "Library/Caches/JetBrains"),
            ("JetBrains logs", "Library/Logs/JetBrains"),
            ("JetBrains caches", ".cache/JetBrains"),
        ] {
            if let Some(c) = global_cache(
                ctx,
                "editors.jetbrains",
                Group::Editors,
                label,
                p.home_join(rel),
            ) {
                out.push(
                    c.regen(Regen::Rebuild { minutes: 5 })
                        .warn(Warning::info(
                            "Indexes are rebuilt on next project open, which takes a few minutes.",
                        ))
                        .build(),
                );
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

pub struct System;

impl Provider for System {
    fn id(&self) -> &'static str {
        "system.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        if has_command("brew") {
            out.push(
                CandidateBuilder::new(
                    "system.homebrew",
                    Group::System,
                    "Homebrew downloads and old versions",
                )
                .path(p.cache_dir().join("Homebrew"))
                .detail("Downloaded bottles and superseded formula versions.")
                .action(Action::Shell {
                    program: "brew".into(),
                    args: vec!["cleanup".into(), "-s".into()],
                })
                .tier(Tier::Safe)
                .regen(redownload("the Homebrew CDN"))
                .build(),
            );
        }

        if has_command("nix-collect-garbage") {
            out.push(
                CandidateBuilder::new("system.nix", Group::System, "Nix store garbage")
                    .path(p.home_join(".nix-profile"))
                    .detail("Store paths no longer reachable from any profile generation.")
                    .action(Action::Shell {
                        program: "nix-collect-garbage".into(),
                        args: vec!["-d".into()],
                    })
                    .tier(Tier::Review)
                    .regen(Regen::Rebuild { minutes: 10 })
                    .warn(Warning::caution("`-d` also deletes old profile generations, so rollback is no longer possible."))
                    .build(),
            );
        }

        for (id, label, rel) in [
            ("system.logs", "Application logs", "Library/Logs"),
            (
                "system.crash-reports",
                "Crash reports",
                "Library/Logs/DiagnosticReports",
            ),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::System, label, p.home_join(rel)) {
                out.push(c.regen(automatic("normal use")).build());
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// ML model caches (disabled by default)
// ---------------------------------------------------------------------------

pub struct MachineLearning;

impl Provider for MachineLearning {
    fn id(&self) -> &'static str {
        "ml.models"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        for (id, label, path, source) in [
            (
                "ml.huggingface",
                "Hugging Face model cache",
                p.cache_dir().join("huggingface"),
                "the Hugging Face Hub",
            ),
            (
                "ml.huggingface",
                "Hugging Face model cache",
                p.home_join(".cache/huggingface"),
                "the Hugging Face Hub",
            ),
            (
                "ml.torch",
                "PyTorch hub cache",
                p.cache_dir().join("torch"),
                "the PyTorch hub",
            ),
            (
                "ml.ollama",
                "Ollama models",
                p.home_join(".ollama/models"),
                "the Ollama registry",
            ),
        ] {
            if !path.exists() {
                continue;
            }
            out.push(
                CandidateBuilder::new(id, Group::Ml, label)
                    .path(path)
                    .detail("Downloaded model weights.")
                    .tier(Tier::Review)
                    .regen(redownload(source))
                    .warn(Warning::caution(
                        "Model weights are very large downloads; re-fetching can take a long time.",
                    ))
                    .build(),
            );
        }

        out
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(DotNet),
        Box::new(Ruby),
        Box::new(Php),
        Box::new(Dart),
        Box::new(BuildTools),
        Box::new(Editors),
        Box::new(System),
        Box::new(MachineLearning),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    fn php_vendor_without_a_lockfile_is_caution() {
        let home = TestHome::new();
        home.project("dev/site", &["composer.json"]);
        home.file("dev/site/vendor/autoload.php", 1024);

        let found = home.discover(&Php);
        let vendor = found
            .iter()
            .find(|c| c.provider == "php.vendor")
            .expect("vendor");
        assert_eq!(vendor.tier, Tier::Caution);
    }

    #[test]
    fn php_vendor_with_a_lockfile_is_reviewable() {
        let home = TestHome::new();
        home.project("dev/site", &["composer.json", "composer.lock"]);
        home.file("dev/site/vendor/autoload.php", 1024);

        let found = home.discover(&Php);
        let vendor = found
            .iter()
            .find(|c| c.provider == "php.vendor")
            .expect("vendor");
        assert_eq!(vendor.tier, Tier::Review);
    }

    #[test]
    fn cmake_build_dirs_are_only_claimed_when_cmake_generated_them() {
        // `build/` is far too common a directory name to claim on sight.
        let home = TestHome::new();
        home.project("dev/native", &["CMakeLists.txt"]);
        home.file("dev/native/build/notes.txt", 64);

        assert!(!home
            .discover(&BuildTools)
            .iter()
            .any(|c| c.provider == "buildtools.cmake-build"));

        home.file("dev/native/build/CMakeCache.txt", 512);
        assert!(home
            .discover(&BuildTools)
            .iter()
            .any(|c| c.provider == "buildtools.cmake-build"));
    }

    #[test]
    fn editor_workspace_storage_warns_about_losing_project_state() {
        let home = TestHome::new();
        let base = if cfg!(target_os = "macos") {
            "Library/Application Support/Code"
        } else {
            ".config/Code"
        };
        home.file(
            &format!("{base}/User/workspaceStorage/abc/state.vscdb"),
            2048,
        );

        let found = home.discover(&Editors);
        let ws = found
            .iter()
            .find(|c| c.provider == "editors.workspace-storage")
            .expect("workspace storage");
        assert_eq!(ws.tier, Tier::Review);
        assert!(ws.warnings.iter().any(|w| w.message.contains("open tabs")));
    }

    #[test]
    fn editor_caches_are_safe() {
        let home = TestHome::new();
        let base = if cfg!(target_os = "macos") {
            "Library/Application Support/Cursor"
        } else {
            ".config/Cursor"
        };
        home.file(&format!("{base}/Cache/data"), 4096);

        let found = home.discover(&Editors);
        let cache = found
            .iter()
            .find(|c| c.label.contains("Cursor"))
            .expect("cursor cache");
        assert_eq!(cache.tier, Tier::Safe);
    }

    #[test]
    fn ml_model_caches_warn_about_the_download_size() {
        let home = TestHome::new();
        home.file(
            ".ollama/models/manifests/registry/library/llama3/latest",
            1024,
        );

        let found = home.discover(&MachineLearning);
        let ollama = found
            .iter()
            .find(|c| c.provider == "ml.ollama")
            .expect("ollama");
        assert_eq!(ollama.tier, Tier::Review);
        assert!(ollama
            .warnings
            .iter()
            .any(|w| w.message.contains("large downloads")));
    }

    #[test]
    fn ml_is_disabled_by_the_default_config() {
        let home = TestHome::new();
        assert!(
            !home.config.providers.is_enabled("ml.models"),
            "model weights are deliberate downloads; opt-in only"
        );
    }

    #[test]
    fn dart_pub_cache_is_review_tier() {
        let home = TestHome::new();
        home.file(".pub-cache/hosted/pub.dev/http-1.0/lib/http.dart", 2048);

        let found = home.discover(&Dart);
        let pub_cache = found
            .iter()
            .find(|c| c.provider == "dart.pub-cache")
            .expect("pub cache");
        assert_eq!(pub_cache.tier, Tier::Review);
    }

    #[test]
    fn empty_home_yields_nothing_from_any_of_these() {
        let home = TestHome::new();
        for provider in providers() {
            let found = home.discover(provider.as_ref());
            // System may still offer brew/nix if they are installed on the host.
            if provider.id() == "system.caches" {
                continue;
            }
            assert!(found.is_empty(), "{} offered {:?}", provider.id(), found);
        }
    }
}
