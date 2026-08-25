//! Local AI tooling.
//!
//! Two shapes of thing live here, both under the `ml` group and disabled by
//! default: model weights are large, deliberate downloads, not incidental
//! build byproducts.
//!
//! - Global model caches with well-known fixed paths: Hugging Face, PyTorch
//!   hub, Ollama, LM Studio.
//! - Git-clone-style installs, found the same way every other project-local
//!   artifact is found — via the stage-1 project walk, refined by a marker
//!   file unique to that tool: ComfyUI, SillyTavern, Automatic1111 /
//!   stable-diffusion-webui.
//!
//! Where a tool ships an HTTP server on a well-known port, a candidate also
//! carries a warning when that port is currently answering, since deleting a
//! model out from under a live server is worse than deleting a cold cache.
//! This is evidence, not a gate: an unreachable port does not withhold the
//! candidate, the same way `containers.rs`'s `daemon_running` gates Docker.

use reclaim_core::model::{Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

// ---------------------------------------------------------------------------
// Global model caches
// ---------------------------------------------------------------------------

pub struct MachineLearning;

impl Provider for MachineLearning {
    fn id(&self) -> &'static str {
        "ml.models"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let check_running = ctx.config.providers.ai.check_running;
        let mut out = Vec::new();

        for (id, label, path, source, running) in [
            (
                "ml.huggingface",
                "Hugging Face model cache",
                p.cache_dir().join("huggingface"),
                "the Hugging Face Hub",
                None,
            ),
            (
                "ml.huggingface",
                "Hugging Face model cache",
                p.home_join(".cache/huggingface"),
                "the Hugging Face Hub",
                None,
            ),
            (
                "ml.torch",
                "PyTorch hub cache",
                p.cache_dir().join("torch"),
                "the PyTorch hub",
                None,
            ),
            (
                "ml.ollama",
                "Ollama models",
                p.home_join(".ollama/models"),
                "the Ollama registry",
                Some(("Ollama", 11_434)),
            ),
            (
                "ml.lmstudio",
                "LM Studio models",
                p.home_join(".lmstudio/models"),
                "the LM Studio model catalog",
                Some(("LM Studio", 1_234)),
            ),
            (
                "ml.lmstudio",
                "LM Studio model cache",
                p.home_join(".cache/lm-studio"),
                "the LM Studio model catalog",
                Some(("LM Studio", 1_234)),
            ),
        ] {
            if !path.exists() {
                continue;
            }
            let mut c = CandidateBuilder::new(id, Group::Ml, label)
                .path(path)
                .detail("Downloaded model weights.")
                .tier(Tier::Review)
                .regen(redownload(source))
                .warn(Warning::caution(
                    "Model weights are very large downloads; re-fetching can take a long time.",
                ));
            if let Some((tool, port)) = running {
                if check_running && port_is_open(port) {
                    c = c.warn(running_warning(tool, port));
                }
            }
            out.push(c.build());
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Git-clone-style installs
// ---------------------------------------------------------------------------

pub struct LocalAiTools;

impl Provider for LocalAiTools {
    fn id(&self) -> &'static str {
        "ml.local-tools"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let mut out = Vec::new();
        out.extend(comfyui_candidates(ctx));
        out.extend(sillytavern_candidates(ctx));
        out.extend(automatic1111_candidates(ctx));
        out
    }
}

/// A `port` that answers right now, if the config allows probing for one.
fn currently_running(ctx: &ScanContext, port: u16) -> bool {
    ctx.config.providers.ai.check_running && port_is_open(port)
}

fn running_warning(tool: &str, port: u16) -> Warning {
    Warning::caution(format!(
        "{tool} appears to be running right now (detected on 127.0.0.1:{port}); files here may be in active use."
    ))
}

/// Warning for generated media: unlike model weights, there is no source to
/// re-download it from.
fn irreplaceable_output_warning() -> Warning {
    Warning::danger("These are your generated outputs, not re-creatable from anywhere else.")
}

fn comfyui_candidates(ctx: &ScanContext) -> Vec<Candidate> {
    let mut out = Vec::new();
    let running = currently_running(ctx, 8_188);

    for project in ctx.projects_with(&["requirements.txt", "pyproject.toml"]) {
        // `.comfy_environment` is a file ComfyUI itself writes at install time;
        // `requirements.txt`/`pyproject.toml` alone match any Python project.
        if !project.path.join(".comfy_environment").is_file() {
            continue;
        }

        if let Some(c) = project_artifact(
            "ml.comfyui-models",
            Group::Ml,
            "ComfyUI models",
            &project.path,
            project.path.join("models"),
        ) {
            let mut c = c
                .detail("Downloaded checkpoints, LoRAs, VAEs and other model weights.")
                .tier(Tier::Review)
                .regen(redownload("Hugging Face / Civitai"))
                .warn(Warning::caution(
                    "Model weights are very large downloads; re-fetching can take a long time.",
                ));
            if running {
                c = c.warn(running_warning("ComfyUI", 8_188));
            }
            out.push(c.build());
        }

        if let Some(c) = project_artifact(
            "ml.comfyui-output",
            Group::Ml,
            "ComfyUI generated output",
            &project.path,
            project.path.join("output"),
        ) {
            out.push(
                c.detail("Images and other media ComfyUI generated.")
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(irreplaceable_output_warning())
                    .build(),
            );
        }
    }

    out
}

fn sillytavern_candidates(ctx: &ScanContext) -> Vec<Candidate> {
    let mut out = Vec::new();
    let running = currently_running(ctx, 8_000);

    for project in ctx.projects_with(&["package.json"]) {
        // `world-info.js` is unique to SillyTavern's frontend; `package.json`
        // alone matches any Node project.
        if !project.path.join("public/scripts/world-info.js").is_file() {
            continue;
        }

        for (id, label, rel) in [
            ("ml.sillytavern-cache", "SillyTavern content cache", "data/_cache"),
            ("ml.sillytavern-cache", "SillyTavern webpack cache", "data/_webpack"),
            ("ml.sillytavern-cache", "SillyTavern upload cache", "data/_uploads"),
        ] {
            if let Some(c) =
                project_artifact(id, Group::Ml, label, &project.path, project.path.join(rel))
            {
                let mut c = c.regen(automatic("normal use"));
                if running {
                    c = c.warn(running_warning("SillyTavern", 8_000));
                }
                out.push(c.build());
            }
        }

        // Thumbnails live per profile (`data/<user>/thumbnails`), and the
        // default profile name can be renamed, so check every profile found.
        for user_dir in subdirs(&project.path.join("data")) {
            if let Some(c) = project_artifact(
                "ml.sillytavern-thumbnails",
                Group::Ml,
                "SillyTavern thumbnail cache",
                &project.path,
                user_dir.join("thumbnails"),
            ) {
                let mut c = c.regen(automatic("next view"));
                if running {
                    c = c.warn(running_warning("SillyTavern", 8_000));
                }
                out.push(c.build());
            }
        }
    }

    out
}

fn automatic1111_candidates(ctx: &ScanContext) -> Vec<Candidate> {
    let mut out = Vec::new();
    let running = currently_running(ctx, 7_860);

    for project in ctx.projects_with(&["requirements.txt", "pyproject.toml"]) {
        // `webui.py` + `modules/paths_internal.py` are stable, distinctive
        // Automatic1111 files; either alone is too generic to trust.
        if !(project.path.join("webui.py").is_file()
            && project.path.join("modules/paths_internal.py").is_file())
        {
            continue;
        }

        if let Some(c) = project_artifact(
            "ml.a1111-models",
            Group::Ml,
            "Stable Diffusion WebUI models",
            &project.path,
            project.path.join("models"),
        ) {
            let mut c = c
                .detail("Checkpoints, LoRAs, VAEs, ControlNet and embedding weights.")
                .tier(Tier::Review)
                .regen(redownload("Hugging Face / Civitai"))
                .warn(Warning::caution(
                    "Model weights are very large downloads; re-fetching can take a long time.",
                ));
            if running {
                c = c.warn(running_warning("Automatic1111", 7_860));
            }
            out.push(c.build());
        }

        if let Some(c) = project_artifact(
            "ml.a1111-output",
            Group::Ml,
            "Stable Diffusion WebUI generated output",
            &project.path,
            project.path.join("outputs"),
        ) {
            out.push(
                c.detail("Images WebUI generated.")
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(irreplaceable_output_warning())
                    .build(),
            );
        }
    }

    out
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(MachineLearning), Box::new(LocalAiTools)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{expect_candidate, TestHome};

    #[test]
    fn ml_model_caches_warn_about_the_download_size() {
        let home = TestHome::new();
        home.file(
            ".ollama/models/manifests/registry/library/llama3/latest",
            1024,
        );

        let found = home.discover(&MachineLearning);
        let ollama = expect_candidate(&found, "ml.ollama");
        assert_eq!(ollama.tier, Tier::Review);
        assert!(ollama
            .warnings
            .iter()
            .any(|w| w.message.contains("large downloads")));
    }

    #[test]
    fn ml_models_is_disabled_by_the_default_config() {
        let home = TestHome::new();
        assert!(
            !home.config.providers.is_enabled("ml.models"),
            "model weights are deliberate downloads; opt-in only"
        );
    }

    #[test]
    fn ml_local_tools_is_disabled_by_the_default_config() {
        let home = TestHome::new();
        assert!(!home.config.providers.is_enabled("ml.local-tools"));
    }

    #[test]
    fn disabling_check_running_never_adds_the_running_warning() {
        let mut home = TestHome::new();
        home.config.providers.ai.check_running = false;
        home.file(".ollama/models/manifests/x", 1024);

        let found = home.discover(&MachineLearning);
        let ollama = expect_candidate(&found, "ml.ollama");
        assert!(!ollama
            .warnings
            .iter()
            .any(|w| w.message.contains("appears to be running")));
    }

    #[test]
    fn comfyui_models_require_the_comfy_environment_marker() {
        let home = TestHome::new();
        // A plain Python project must not be mistaken for a ComfyUI install.
        home.project("dev/some-python-app", &["requirements.txt"]);
        home.dir("dev/some-python-app/models");
        assert!(home.discover(&LocalAiTools).is_empty());

        home.file("dev/some-python-app/.comfy_environment", 8);
        let found = home.discover(&LocalAiTools);
        expect_candidate(&found, "ml.comfyui-models");
    }

    #[test]
    fn comfyui_output_is_caution_and_never_regenerates() {
        let home = TestHome::new();
        home.project("dev/comfy", &["requirements.txt"]);
        home.file("dev/comfy/.comfy_environment", 8);
        home.dir("dev/comfy/output");

        let found = home.discover(&LocalAiTools);
        let output = expect_candidate(&found, "ml.comfyui-output");
        assert_eq!(output.tier, Tier::Caution);
        assert_eq!(output.regen, Regen::Never);
        assert!(output
            .warnings
            .iter()
            .any(|w| w.severity == reclaim_core::Severity::Danger));
    }

    #[test]
    fn sillytavern_requires_the_world_info_marker() {
        let home = TestHome::new();
        // A plain Node project must not be mistaken for SillyTavern.
        home.project("dev/some-node-app", &["package.json"]);
        home.file("dev/some-node-app/data/_cache/x", 8);
        assert!(home.discover(&LocalAiTools).is_empty());

        home.file("dev/some-node-app/public/scripts/world-info.js", 8);
        let found = home.discover(&LocalAiTools);
        expect_candidate(&found, "ml.sillytavern-cache");
    }

    #[test]
    fn sillytavern_thumbnails_are_found_per_profile() {
        let home = TestHome::new();
        home.project("dev/st", &["package.json"]);
        home.file("dev/st/public/scripts/world-info.js", 8);
        home.file("dev/st/data/default-user/thumbnails/avatar.png", 8);
        home.file("dev/st/data/second-user/thumbnails/avatar.png", 8);

        let found = home.discover(&LocalAiTools);
        let thumb_paths: Vec<_> = found
            .iter()
            .filter(|c| c.provider == "ml.sillytavern-thumbnails")
            .flat_map(|c| c.paths.clone())
            .collect();
        assert_eq!(thumb_paths.len(), 2, "one candidate per profile: {found:#?}");
    }

    #[test]
    fn a1111_requires_both_distinctive_files() {
        let home = TestHome::new();
        home.project("dev/sd-webui", &["requirements.txt"]);
        home.file("dev/sd-webui/webui.py", 8);
        home.dir("dev/sd-webui/models");
        // `modules/paths_internal.py` missing: not yet claimed.
        assert!(home.discover(&LocalAiTools).is_empty());

        home.file("dev/sd-webui/modules/paths_internal.py", 8);
        let found = home.discover(&LocalAiTools);
        expect_candidate(&found, "ml.a1111-models");
    }

    #[test]
    fn an_empty_home_yields_nothing() {
        let home = TestHome::new();
        assert!(home.discover(&MachineLearning).is_empty());
        assert!(home.discover(&LocalAiTools).is_empty());
    }
}
