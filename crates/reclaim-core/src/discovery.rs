//! Stage 1: find the user's projects with a single parallel walk.
//!
//! The naive design has every provider crawling the disk for its own markers,
//! which walks `~/dev` a dozen times over. Instead this runs **one** pruned walk
//! and hands every provider the same [`ProjectRoot`] list to match against.
//!
//! Pruning is what makes it fast: we never descend into `node_modules`, `target`,
//! `.git` or the other artifact directories, which is where essentially all the
//! inodes live. A project root is identified by its marker file, and the artifacts
//! inside it are then derived by path rather than by searching.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::ScanConfig;
use crate::model::ProjectRoot;

/// Marker files that identify a project root, and the group they imply.
///
/// Order matters only for readability; a directory can carry several markers
/// (a React Native app has `package.json`, `Podfile` and `build.gradle`).
pub const PROJECT_MARKERS: &[&str] = &[
    // Node
    "package.json",
    // Rust
    "Cargo.toml",
    // Python
    "pyproject.toml",
    "requirements.txt",
    "Pipfile",
    "setup.py",
    "environment.yml",
    // JVM
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "build.sbt",
    // Go
    "go.mod",
    // Apple
    "Podfile",
    "Package.swift",
    "Cartfile",
    // .NET
    "Directory.Build.props",
    // Ruby
    "Gemfile",
    // PHP
    "composer.json",
    // Dart / Flutter
    "pubspec.yaml",
    // Build tools
    "CMakeLists.txt",
    "WORKSPACE",
    "MODULE.bazel",
];

/// Directory names never descended into during the project walk.
///
/// These are artifact and dependency trees. They hold the overwhelming majority
/// of files on a developer machine, and nothing inside them is ever a project root.
const NEVER_DESCEND: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    "vendor",
    "Pods",
    "Carthage",
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    ".tox",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".gradle",
    ".dart_tool",
    ".terraform",
    "DerivedData",
    ".Trash",
    "Library",
    ".cache",
    "obj",
    "bin",
];

/// Result of the project walk.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub projects: Vec<ProjectRoot>,
    /// Directories skipped because of a permission error, so the UI can say the
    /// list may be incomplete rather than implying it is exhaustive.
    pub unreadable: Vec<PathBuf>,
}

/// Walk `roots` in parallel and return every project directory found.
///
/// A directory carrying a marker is recorded but **still descended into**, because
/// monorepos nest real projects under a root `package.json`.
pub fn find_projects(roots: &[PathBuf], config: &ScanConfig) -> Discovery {
    let excludes = compile_excludes(&config.exclude);
    let projects = Mutex::new(Vec::new());
    let unreadable = Mutex::new(Vec::new());

    for root in roots {
        if !root.is_dir() {
            continue;
        }

        let walker = jwalk::WalkDirGeneric::<((), ())>::new(root)
            .parallelism(jwalk::Parallelism::RayonNewPool(config.threads()))
            .skip_hidden(false)
            .follow_links(config.follow_symlinks)
            .max_depth(config.max_depth)
            .process_read_dir({
                let excludes = excludes.clone();
                move |depth, _path, _state, children| {
                    // jwalk calls this once for the root itself with `depth: None`.
                    // The prune rules below describe which *children* to skip; applying
                    // them to the root would silently scan nothing whenever the root is
                    // hidden (`~/.local/src`) or named like an artifact dir (`build`).
                    // The user asked for this root explicitly, so it is always in.
                    if depth.is_none() {
                        return;
                    }
                    children.retain(|child| {
                        let Ok(child) = child else { return true };
                        if !child.file_type().is_dir() {
                            return true;
                        }
                        let name = child.file_name().to_string_lossy();
                        if NEVER_DESCEND.contains(&name.as_ref()) {
                            return false;
                        }
                        // Hidden directories other than the known artifact ones are
                        // dotfile repos and editor state; nothing worth scanning.
                        if name.starts_with('.') && name != ".config" {
                            return false;
                        }
                        !is_excluded(&child.path(), &excludes)
                    });
                }
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

            if !entry.file_type().is_dir() {
                continue;
            }

            let dir = entry.path();
            let markers = markers_in(&dir);
            if !markers.is_empty() {
                projects
                    .lock()
                    .unwrap()
                    .push(ProjectRoot { path: dir, markers });
            }
        }
    }

    let mut projects = projects.into_inner().unwrap();
    projects.sort_by(|a, b| a.path.cmp(&b.path));
    projects.dedup_by(|a, b| a.path == b.path);

    Discovery {
        projects,
        unreadable: unreadable.into_inner().unwrap(),
    }
}

/// Which marker files a directory contains.
fn markers_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // `*.xcodeproj` and `*.xcworkspace` are directories, not files, so they
            // are matched by extension rather than against the fixed marker list.
            if PROJECT_MARKERS.contains(&name.as_str()) {
                return Some(name);
            }
            let path = e.path();
            let ext = path.extension().and_then(|e| e.to_str());
            matches!(
                ext,
                Some("xcodeproj") | Some("xcworkspace") | Some("csproj") | Some("sln")
            )
            .then_some(name)
        })
        .collect();

    found.sort();
    found.dedup();
    found
}

fn compile_excludes(patterns: &[String]) -> Vec<glob::Pattern> {
    patterns
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect()
}

fn is_excluded(path: &Path, excludes: &[glob::Pattern]) -> bool {
    excludes.iter().any(|p| p.matches_path(path))
}

/// Project roots that carry any of the given markers.
pub fn projects_with(projects: &[ProjectRoot], markers: &[&str]) -> Vec<ProjectRoot> {
    projects
        .iter()
        .filter(|p| p.has_any_marker(markers))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(base: &Path, rel: &str) {
        let p = base.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"{}").unwrap();
    }

    fn config() -> ScanConfig {
        ScanConfig {
            exclude: Vec::new(),
            max_depth: 8,
            concurrency: 2,
            ..Default::default()
        }
    }

    #[test]
    fn finds_projects_by_their_marker_files() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "web-app/package.json");
        touch(tmp.path(), "rust-tool/Cargo.toml");
        touch(tmp.path(), "notes/readme.md");

        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        let names: Vec<String> = found
            .projects
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"web-app".to_string()));
        assert!(names.contains(&"rust-tool".to_string()));
        assert!(
            !names.contains(&"notes".to_string()),
            "a plain folder is not a project"
        );
    }

    #[test]
    fn records_every_marker_in_a_polyglot_project() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "rn-app/package.json");
        touch(tmp.path(), "rn-app/ios/Podfile");
        touch(tmp.path(), "rn-app/android/build.gradle");

        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        let root = found
            .projects
            .iter()
            .find(|p| p.path.ends_with("rn-app"))
            .unwrap();
        assert!(root.has_marker("package.json"));
        // The nested ios/android dirs are separate roots with their own markers.
        assert!(found.projects.iter().any(|p| p.has_marker("Podfile")));
        assert!(found.projects.iter().any(|p| p.has_marker("build.gradle")));
    }

    #[test]
    fn never_descends_into_artifact_directories() {
        // A `package.json` inside node_modules is a dependency, not a project of
        // the user's, and there are tens of thousands of them.
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "app/package.json");
        touch(tmp.path(), "app/node_modules/lodash/package.json");
        touch(tmp.path(), "app/node_modules/react/package.json");

        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        assert_eq!(found.projects.len(), 1, "found: {:?}", found.projects);
        assert!(found.projects[0].path.ends_with("app"));
    }

    #[test]
    fn descends_into_a_monorepo_below_its_root_marker() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "mono/package.json");
        touch(tmp.path(), "mono/packages/api/package.json");
        touch(tmp.path(), "mono/packages/web/package.json");

        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        assert_eq!(
            found.projects.len(),
            3,
            "workspace members are projects too"
        );
    }

    #[test]
    fn respects_the_configured_exclude_globs() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "keep/Cargo.toml");
        touch(tmp.path(), "skip/Cargo.toml");

        let cfg = ScanConfig {
            exclude: vec![format!("{}/skip*", tmp.path().display())],
            ..config()
        };
        let found = find_projects(&[tmp.path().to_path_buf()], &cfg);
        assert_eq!(found.projects.len(), 1);
        assert!(found.projects[0].path.ends_with("keep"));
    }

    #[test]
    fn respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a/b/c/d/e/f/deep/Cargo.toml");
        let cfg = ScanConfig {
            max_depth: 3,
            ..config()
        };
        let found = find_projects(&[tmp.path().to_path_buf()], &cfg);
        assert!(
            found.projects.is_empty(),
            "should not have reached the deep project"
        );
    }

    #[test]
    fn skips_hidden_directories() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), ".dotfiles/package.json");
        touch(tmp.path(), "real/package.json");
        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        assert_eq!(found.projects.len(), 1);
        assert!(found.projects[0].path.ends_with("real"));
    }

    #[test]
    fn matches_xcode_project_directories_by_extension() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("ios-app/MyApp.xcodeproj")).unwrap();
        let found = find_projects(&[tmp.path().to_path_buf()], &config());
        let root = found.projects.iter().find(|p| p.path.ends_with("ios-app"));
        assert!(
            root.is_some(),
            "xcodeproj is a directory, not a file: {:?}",
            found.projects
        );
    }

    #[test]
    fn scans_a_root_whose_own_name_would_otherwise_be_pruned() {
        // The prune rules apply to children, never to the root the user asked for.
        // Regression: a hidden root (`~/.local/src`) or one named `build` used to
        // filter itself out and silently scan nothing.
        for root_name in [".hidden-root", "build", "node_modules"] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().join(root_name);
            touch(&root, "app/Cargo.toml");

            let found = find_projects(&[root], &config());
            assert_eq!(
                found.projects.len(),
                1,
                "root named `{root_name}` must still be scanned"
            );
        }
    }

    #[test]
    fn a_missing_root_is_skipped_silently() {
        let found = find_projects(&[PathBuf::from("/definitely/not/here")], &config());
        assert!(found.projects.is_empty());
    }

    #[test]
    fn results_are_sorted_and_deduplicated() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "b/Cargo.toml");
        touch(tmp.path(), "a/Cargo.toml");
        // The same root listed twice must not yield duplicate projects.
        let found = find_projects(
            &[tmp.path().to_path_buf(), tmp.path().to_path_buf()],
            &config(),
        );
        assert_eq!(found.projects.len(), 2);
        assert!(found.projects[0].path < found.projects[1].path);
    }

    #[test]
    fn projects_with_filters_by_marker() {
        let projects = vec![
            ProjectRoot {
                path: "/a".into(),
                markers: vec!["package.json".into()],
            },
            ProjectRoot {
                path: "/b".into(),
                markers: vec!["Cargo.toml".into()],
            },
        ];
        let node = projects_with(&projects, &["package.json"]);
        assert_eq!(node.len(), 1);
        assert_eq!(node[0].path, PathBuf::from("/a"));
    }
}
