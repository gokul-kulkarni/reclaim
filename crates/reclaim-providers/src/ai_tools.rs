//! Local AI tooling.
//!
//! Three shapes of thing live here, all under the `ml` group and disabled by
//! default: model weights are large, deliberate downloads, not incidental
//! build byproducts.
//!
//! - Global model caches with well-known fixed paths: Hugging Face, PyTorch
//!   hub, LM Studio.
//! - Ollama, model by model rather than as one directory — its storage is
//!   content-addressed, so "delete everything Ollama ever pulled" is rarely
//!   what anyone wants. See `OllamaModels` below.
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use reclaim_core::model::{Action, Candidate, CandidateBuilder, Group, Kind, Regen, Tier, Warning};
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
// Ollama, model by model
// ---------------------------------------------------------------------------
//
// Ollama's storage is content-addressed: each `manifests/<registry>/<namespace>/
// <model>/<tag>` file lists the `blobs/sha256-<digest>` files that make it up, and
// a blob is not necessarily private to one model (two tags of the same pull, or a
// shared base layer, reference the same blob file rather than a hardlinked copy).
// So unlike everything else in this crate, each model here gets its own candidate,
// sized from exactly the blobs no other installed model needs, and removed by
// shelling out to `ollama rm` rather than deleting paths — Ollama itself is what
// knows how to drop a model without corrupting one that shares a layer with it.

#[derive(Deserialize)]
struct OllamaManifestEntry {
    digest: String,
    size: u64,
}

#[derive(Deserialize)]
struct OllamaManifest {
    config: OllamaManifestEntry,
    #[serde(default)]
    layers: Vec<OllamaManifestEntry>,
}

struct OllamaModel {
    manifest_path: PathBuf,
    reference: String,
    /// digest -> size, config and layers combined, deduplicated by digest.
    digests: BTreeMap<String, u64>,
}

pub struct OllamaModels;

impl Provider for OllamaModels {
    fn id(&self) -> &'static str {
        "ml.ollama"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let models_dir = ctx.paths.home_join(".ollama/models");
        let manifests_dir = models_dir.join("manifests");
        if !manifests_dir.is_dir() {
            return Vec::new();
        }

        let mut models: Vec<OllamaModel> = find_files(&manifests_dir)
            .into_iter()
            .filter_map(|manifest_path| {
                let text = std::fs::read_to_string(&manifest_path).ok()?;
                let manifest: OllamaManifest = serde_json::from_str(&text).ok()?;
                let rel = manifest_path.strip_prefix(&manifests_dir).ok()?;
                let reference = ollama_reference(rel)?;

                let mut digests = BTreeMap::new();
                digests.insert(manifest.config.digest, manifest.config.size);
                for layer in manifest.layers {
                    digests.insert(layer.digest, layer.size);
                }

                Some(OllamaModel {
                    manifest_path,
                    reference,
                    digests,
                })
            })
            .collect();

        if models.is_empty() {
            return Vec::new();
        }
        models.sort_by(|a, b| a.reference.cmp(&b.reference));

        // How many installed models reference each digest, so a shared layer is
        // never counted as "freed" by deleting just one of them.
        let mut referrers: HashMap<String, usize> = HashMap::new();
        for model in &models {
            for digest in model.digests.keys() {
                *referrers.entry(digest.clone()).or_insert(0) += 1;
            }
        }

        let blobs_dir = models_dir.join("blobs");
        let running = currently_running(ctx, 11_434);
        let loaded = ollama_loaded_models(ctx);

        models
            .into_iter()
            .map(|model| {
                let mut paths = vec![model.manifest_path.clone()];
                let mut shared_bytes = 0u64;
                for (digest, size) in &model.digests {
                    if referrers[digest.as_str()] > 1 {
                        shared_bytes += size;
                        continue;
                    }
                    let blob = blobs_dir.join(digest.replace(':', "-"));
                    if blob.exists() {
                        paths.push(blob);
                    }
                }

                let mut c = CandidateBuilder::new("ml.ollama", Group::Ml, &model.reference)
                    .paths(paths)
                    .kind(Kind::Command)
                    .detail("Ollama model weights.")
                    .tier(Tier::Review)
                    .regen(redownload("the Ollama registry"))
                    .action(Action::Shell {
                        program: "ollama".into(),
                        args: vec!["rm".into(), model.reference.clone()],
                    })
                    .warn(Warning::caution(
                        "Model weights are very large downloads; re-fetching can take a long time.",
                    ));

                if shared_bytes > 0 {
                    c = c.warn(Warning::caution(format!(
                        "Shares {} with other installed models; removing this model may not free that portion.",
                        reclaim_core::format::bytes(shared_bytes)
                    )));
                }
                if running {
                    c = c.warn(running_warning("Ollama", 11_434));
                }
                if loaded.contains(&model.reference) {
                    c = c.warn(Warning::caution(
                        "This model is currently loaded in Ollama's memory (`ollama ps`); \
                         removing it while in use may disrupt whatever is using it right now.",
                    ));
                }

                c.build()
            })
            .collect()
    }
}

/// Every regular file under `dir`, recursively. Manifest trees are always tiny
/// (a handful of models), so a plain `read_dir` recursion is fine here — this is
/// not the pipeline's big parallel walk.
fn find_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The model reference Ollama itself uses (`ollama list`'s NAME column, and what
/// `ollama rm`/`ollama ps` expect), reconstructed from a manifest's path relative
/// to `manifests/`: `<registry>/<namespace...>/<model>/<tag>`.
///
/// `registry.ollama.ai/library` is the default registry and namespace, and is
/// elided the same way `ollama list` elides it (`library/llama3.2/latest` shows as
/// `llama3.2:latest`); anything else keeps its full path
/// (`hf.co/bartowski/dolphin-.../Q4_K_M` shows as `hf.co/bartowski/dolphin-...:Q4_K_M`).
fn ollama_reference(rel: &Path) -> Option<String> {
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let parts: &[&str] =
        if parts.first() == Some(&"registry.ollama.ai") && parts.get(1) == Some(&"library") {
            &parts[2..]
        } else {
            &parts[..]
        };
    let (tag, model) = parts.split_last()?;
    if model.is_empty() {
        return None;
    }
    Some(format!("{}:{tag}", model.join("/")))
}

/// Models currently loaded into Ollama's memory, per `ollama ps`. Best-effort: a
/// missing binary, an unreachable server, or a parse miss just means "none known
/// to be loaded," never an error.
fn ollama_loaded_models(ctx: &ScanContext) -> HashSet<String> {
    if !ctx.config.providers.ai.check_running || !has_command("ollama") {
        return HashSet::new();
    }
    match std::process::Command::new("ollama").arg("ps").output() {
        Ok(out) if out.status.success() => parse_ollama_ps(&String::from_utf8_lossy(&out.stdout)),
        _ => HashSet::new(),
    }
}

/// Parse `ollama ps`'s plain-text table into the set of loaded model references.
/// Model references never contain whitespace, so the first field of each row
/// (after the header) is all that's needed.
fn parse_ollama_ps(text: &str) -> HashSet<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
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
            (
                "ml.sillytavern-cache",
                "SillyTavern content cache",
                "data/_cache",
            ),
            (
                "ml.sillytavern-cache",
                "SillyTavern webpack cache",
                "data/_webpack",
            ),
            (
                "ml.sillytavern-cache",
                "SillyTavern upload cache",
                "data/_uploads",
            ),
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
    vec![
        Box::new(MachineLearning),
        Box::new(OllamaModels),
        Box::new(LocalAiTools),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{expect_candidate, TestHome};

    #[test]
    fn ml_model_caches_warn_about_the_download_size() {
        let home = TestHome::new();
        home.file(".lmstudio/models/x", 1024);

        let found = home.discover(&MachineLearning);
        let lmstudio = expect_candidate(&found, "ml.lmstudio");
        assert_eq!(lmstudio.tier, Tier::Review);
        assert!(lmstudio
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
    fn ml_ollama_is_disabled_by_the_default_config() {
        let home = TestHome::new();
        assert!(!home.config.providers.is_enabled("ml.ollama"));
    }

    #[test]
    fn ollama_reference_elides_the_default_registry_and_namespace() {
        assert_eq!(
            ollama_reference(Path::new("registry.ollama.ai/library/llama3.2/latest")),
            Some("llama3.2:latest".to_string())
        );
    }

    #[test]
    fn ollama_reference_keeps_the_full_path_for_other_registries() {
        assert_eq!(
            ollama_reference(Path::new(
                "hf.co/bartowski/dolphin-2.9.3-mistral-7B-32k-GGUF/Q4_K_M"
            )),
            Some("hf.co/bartowski/dolphin-2.9.3-mistral-7B-32k-GGUF:Q4_K_M".to_string())
        );
    }

    #[test]
    fn parse_ollama_ps_reads_the_name_column() {
        let table = "NAME                                                        ID              SIZE      PROCESSOR    CONTEXT    UNTIL   \n\
                      hf.co/bartowski/dolphin-2.9.3-mistral-7B-32k-GGUF:Q4_K_M    4c90ae589154    5.5 GB    100% GPU     8192       Forever    \n";
        let loaded = parse_ollama_ps(table);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains("hf.co/bartowski/dolphin-2.9.3-mistral-7B-32k-GGUF:Q4_K_M"));
    }

    #[test]
    fn parse_ollama_ps_on_an_empty_table_is_empty() {
        assert!(
            parse_ollama_ps("NAME    ID    SIZE    PROCESSOR    CONTEXT    UNTIL\n").is_empty()
        );
    }

    fn write_manifest(home: &TestHome, rel: &str, json: &str) {
        let path = home.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json).unwrap();
    }

    #[test]
    fn ollama_offers_one_candidate_per_model_reclaimed_via_ollama_rm() {
        let home = TestHome::new();
        write_manifest(
            &home,
            ".ollama/models/manifests/registry.ollama.ai/library/llama3.2/latest",
            r#"{"config":{"digest":"sha256:cfg1","size":420},"layers":[{"digest":"sha256:layer1","size":2000}]}"#,
        );
        home.file(".ollama/models/blobs/sha256-cfg1", 8);
        home.file(".ollama/models/blobs/sha256-layer1", 8);

        let found = home.discover(&OllamaModels);
        let model = expect_candidate(&found, "ml.ollama");
        assert_eq!(model.label, "llama3.2:latest");
        assert_eq!(model.kind, Kind::Command);
        assert_eq!(
            model.action,
            Action::Shell {
                program: "ollama".into(),
                args: vec!["rm".into(), "llama3.2:latest".into()],
            }
        );
        assert!(model.paths.iter().any(|p| p.ends_with("sha256-layer1")));
    }

    #[test]
    fn a_shared_layer_is_not_offered_for_deletion_by_either_model() {
        let home = TestHome::new();
        write_manifest(
            &home,
            ".ollama/models/manifests/registry.ollama.ai/library/model-a/latest",
            r#"{"config":{"digest":"sha256:cfg-a","size":100},"layers":[{"digest":"sha256:shared","size":5000}]}"#,
        );
        write_manifest(
            &home,
            ".ollama/models/manifests/registry.ollama.ai/library/model-b/latest",
            r#"{"config":{"digest":"sha256:cfg-b","size":100},"layers":[{"digest":"sha256:shared","size":5000}]}"#,
        );
        home.file(".ollama/models/blobs/sha256-cfg-a", 8);
        home.file(".ollama/models/blobs/sha256-cfg-b", 8);
        home.file(".ollama/models/blobs/sha256-shared", 8);

        let found = home.discover(&OllamaModels);
        assert_eq!(found.len(), 2);
        for model in &found {
            assert!(
                !model.paths.iter().any(|p| p.ends_with("sha256-shared")),
                "{} should not claim the shared blob",
                model.label
            );
            assert!(model.warnings.iter().any(|w| w.message.contains("Shares")));
        }
    }

    #[test]
    fn disabling_check_running_suppresses_ollamas_running_and_loaded_warnings() {
        let mut home = TestHome::new();
        home.config.providers.ai.check_running = false;
        write_manifest(
            &home,
            ".ollama/models/manifests/registry.ollama.ai/library/llama3.2/latest",
            r#"{"config":{"digest":"sha256:cfg1","size":420},"layers":[]}"#,
        );
        home.file(".ollama/models/blobs/sha256-cfg1", 8);

        let found = home.discover(&OllamaModels);
        let model = expect_candidate(&found, "ml.ollama");
        assert!(!model
            .warnings
            .iter()
            .any(|w| w.message.contains("appears to be running")
                || w.message.contains("currently loaded")));
    }

    #[test]
    fn disabling_check_running_never_adds_the_running_warning() {
        let mut home = TestHome::new();
        home.config.providers.ai.check_running = false;
        home.file(".lmstudio/models/x", 1024);

        let found = home.discover(&MachineLearning);
        let lmstudio = expect_candidate(&found, "ml.lmstudio");
        assert!(!lmstudio
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
        assert_eq!(
            thumb_paths.len(),
            2,
            "one candidate per profile: {found:#?}"
        );
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
        assert!(home.discover(&OllamaModels).is_empty());
        assert!(home.discover(&LocalAiTools).is_empty());
    }
}
