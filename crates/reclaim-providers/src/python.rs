//! Python: package caches, virtualenvs and tool caches.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

const MARKERS: &[&str] = &[
    "pyproject.toml",
    "requirements.txt",
    "Pipfile",
    "setup.py",
    "environment.yml",
];

/// Files from which a virtualenv can be faithfully rebuilt.
const DEPENDENCY_SPECS: &[&str] = &[
    "requirements.txt",
    "pyproject.toml",
    "Pipfile.lock",
    "poetry.lock",
    "uv.lock",
    "environment.yml",
    "setup.py",
];

pub struct PythonCaches;

impl Provider for PythonCaches {
    fn id(&self) -> &'static str {
        "python.caches"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        for (id, label, path) in [
            (
                "python.pip-cache",
                "pip download cache",
                p.cache_dir().join("pip"),
            ),
            ("python.uv-cache", "uv cache", p.cache_dir().join("uv")),
            (
                "python.poetry-cache",
                "Poetry cache",
                p.cache_dir().join("pypoetry"),
            ),
            (
                "python.pipenv-cache",
                "Pipenv cache",
                p.cache_dir().join("pipenv"),
            ),
            (
                "python.pre-commit",
                "pre-commit hook environments",
                p.cache_dir().join("pre-commit"),
            ),
        ] {
            if let Some(c) = global_cache(ctx, id, Group::Python, label, path) {
                out.push(c.regen(redownload("PyPI")).build());
            }
        }

        // macOS keeps pip's cache under ~/Library/Caches; the XDG path above misses it.
        if let Some(c) = global_cache(
            ctx,
            "python.pip-cache",
            Group::Python,
            "pip download cache",
            p.home_join(".cache/pip"),
        ) {
            out.push(c.regen(redownload("PyPI")).build());
        }

        // Conda's package dir is hardlinked into every environment, exactly like
        // the pnpm store. Deleting it does not corrupt the environments, but it
        // does mean the next `conda create` re-downloads everything.
        for conda in ["miniconda3", "anaconda3", "miniforge3", "mambaforge"] {
            let pkgs = p.home_join(conda).join("pkgs");
            if !pkgs.exists() {
                continue;
            }
            out.push(
                CandidateBuilder::new(
                    "python.conda-pkgs",
                    Group::Python,
                    format!("{conda} package cache"),
                )
                .path(pkgs)
                .detail("Extracted conda packages, hardlinked into each environment.")
                .tier(Tier::Review)
                .regen(redownload("the conda channels"))
                .warn(hardlink_store_warning("conda environment"))
                .build(),
            );
        }

        if let Some(c) = global_cache(
            ctx,
            "python.poetry-venvs",
            Group::Python,
            "Poetry virtualenvs",
            p.cache_dir().join("pypoetry/virtualenvs"),
        ) {
            out.push(
                c.detail("Poetry keeps per-project environments here rather than in the project.")
                    .tier(Tier::Review)
                    .regen(Regen::Rebuild { minutes: 3 })
                    .warn(Warning::info(
                        "Recreated by `poetry install` in the affected project.",
                    ))
                    .build(),
            );
        }

        out
    }
}

pub struct PythonProjects;

impl Provider for PythonProjects {
    fn id(&self) -> &'static str {
        "python.projects"
    }

    fn markers(&self) -> &'static [&'static str] {
        MARKERS
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        artifacts_in_projects(ctx, MARKERS, |project| {
            let mut out = Vec::new();
            let reproducible = any_file_exists(&project.path, DEPENDENCY_SPECS);

            // Virtualenvs are big and rebuildable, but only if the project actually
            // records its dependencies somewhere.
            for venv in [".venv", "venv", "env", ".virtualenv"] {
                let Some(c) = project_artifact(
                    "python.venv",
                    Group::Python,
                    "virtualenv",
                    &project.path,
                    project.path.join(venv),
                ) else {
                    continue;
                };
                // A directory is only a virtualenv if it has the marker file; `env/`
                // is otherwise a very common name for ordinary source directories.
                if !project.path.join(venv).join("pyvenv.cfg").exists() {
                    continue;
                }
                out.push(
                    c.detail(format!("`{venv}` in this project."))
                        .tier(if reproducible {
                            Tier::Review
                        } else {
                            Tier::Caution
                        })
                        .regen(if reproducible {
                            Regen::Rebuild { minutes: 2 }
                        } else {
                            Regen::Never
                        })
                        .warn_if(
                            !reproducible,
                            Warning::caution(
                                "No requirements.txt, pyproject.toml or lockfile in this project, \
                                 so the installed packages are not recorded anywhere else.",
                            ),
                        )
                        .build(),
                );
            }

            for (dir, label) in [
                (".tox", "tox environments"),
                (".nox", "nox environments"),
                (".mypy_cache", "mypy cache"),
                (".pytest_cache", "pytest cache"),
                (".ruff_cache", "Ruff cache"),
                (".coverage_cache", "coverage cache"),
                ("__pycache__", "bytecode cache"),
                (".ipynb_checkpoints", "notebook checkpoints"),
            ] {
                if let Some(c) = project_artifact(
                    "python.tool-cache",
                    Group::Python,
                    label,
                    &project.path,
                    project.path.join(dir),
                ) {
                    out.push(c.regen(automatic("the next run")).build());
                }
            }

            out
        })
    }
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(PythonCaches), Box::new(PythonProjects)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    fn finds_the_pip_cache() {
        let home = TestHome::new();
        home.file(".cache/pip/wheels/abc.whl", 4096);
        let found = home.discover(&PythonCaches);
        assert!(found.iter().any(|c| c.provider == "python.pip-cache"));
    }

    #[test]
    fn conda_packages_warn_about_hardlinks() {
        let home = TestHome::new();
        home.file("miniconda3/pkgs/numpy-1.0/info.json", 512);

        let found = home.discover(&PythonCaches);
        let pkgs = found
            .iter()
            .find(|c| c.provider == "python.conda-pkgs")
            .expect("conda pkgs");
        assert_eq!(pkgs.tier, Tier::Review);
        assert!(pkgs
            .warnings
            .iter()
            .any(|w| w.message.contains("hardlinked")));
    }

    #[test]
    fn a_venv_with_a_requirements_file_is_rebuildable() {
        let home = TestHome::new();
        home.project("dev/svc", &["requirements.txt"]);
        home.file("dev/svc/.venv/pyvenv.cfg", 64);
        home.file("dev/svc/.venv/lib/python3.13/site-packages/x.py", 2048);

        let found = home.discover(&PythonProjects);
        let venv = found
            .iter()
            .find(|c| c.provider == "python.venv")
            .expect("venv");
        assert_eq!(venv.tier, Tier::Review);
        assert!(matches!(venv.regen, Regen::Rebuild { .. }));
    }

    #[test]
    fn a_venv_with_no_dependency_spec_is_caution_and_unrecoverable() {
        // The installed packages exist nowhere else; deleting is a one-way door.
        let home = TestHome::new();
        home.project("dev/scratch", &["setup.py"]);
        std::fs::remove_file(home.path("dev/scratch/setup.py")).unwrap();
        home.file("dev/scratch/.venv/pyvenv.cfg", 64);

        let found = home.discover(&PythonProjects);
        let venv = found
            .iter()
            .find(|c| c.provider == "python.venv")
            .expect("venv");
        assert_eq!(venv.tier, Tier::Caution);
        assert_eq!(venv.regen, Regen::Never);
    }

    #[test]
    fn a_source_directory_named_env_is_not_mistaken_for_a_virtualenv() {
        // `env/` holding config code is common; only pyvenv.cfg makes it an env.
        let home = TestHome::new();
        home.project("dev/app", &["requirements.txt"]);
        home.file("dev/app/env/settings.py", 512);

        let found = home.discover(&PythonProjects);
        assert!(!found.iter().any(|c| c.provider == "python.venv"));
    }

    #[test]
    fn finds_tool_caches() {
        let home = TestHome::new();
        home.project("dev/app", &["pyproject.toml"]);
        home.file("dev/app/.mypy_cache/3.13/x.json", 1024);
        home.file("dev/app/.pytest_cache/v/cache/lastfailed", 128);

        let found = home.discover(&PythonProjects);
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"mypy cache"));
        assert!(labels.contains(&"pytest cache"));
    }

    #[test]
    fn nothing_is_offered_on_a_machine_without_python() {
        let home = TestHome::new();
        assert!(home.discover(&PythonCaches).is_empty());
        assert!(home.discover(&PythonProjects).is_empty());
    }
}
