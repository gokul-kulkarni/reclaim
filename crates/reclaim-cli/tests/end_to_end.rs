//! End-to-end tests driving the real `reclaim` binary.
//!
//! Every test runs against a sandbox home via `--root`, so nothing here can touch
//! the developer's actual caches. These cover the contract a user actually
//! depends on: exit codes, what gets deleted, and what is refused.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// A sandbox home with a realistic set of caches and projects.
struct Sandbox {
    _tmp: TempDir,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        Self { _tmp: tmp, home }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn file(&self, rel: &str, bytes: usize) -> &Self {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        self
    }

    fn config(&self, contents: &str) -> &Self {
        self.file(".config/reclaim/config.toml", 0);
        std::fs::write(self.path(".config/reclaim/config.toml"), contents).unwrap();
        self
    }

    /// A populated home: a global cache plus a project with a lockfile.
    fn populated() -> Self {
        let sandbox = Self::new();
        sandbox
            .file(".npm/_cacache/blob", 2_000_000)
            .file(".gradle/caches/modules-2/lib.jar", 3_000_000)
            .file("dev/app/package.json", 32)
            .file("dev/app/package-lock.json", 32)
            .file("dev/app/node_modules/pkg/index.js", 4_000_000)
            .config("[scan]\nproject_roots = [\"~/dev\"]\n");
        sandbox
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("reclaim").unwrap();
        cmd.arg("--root").arg(&self.home).arg("--no-color");
        // Keep the ambient environment from leaking into a sandboxed run.
        cmd.env_remove("RECLAIM_MIN_SIZE")
            .env_remove("RECLAIM_DELETE_MODE")
            .env_remove("RECLAIM_PROJECT_ROOTS");
        cmd
    }
}

/// Flags that relax the size, age and in-use filters. The fixtures are small and
/// seconds old, so without these the protections that matter on a real machine
/// correctly hide everything.
const PERMISSIVE: [&str; 3] = ["--min-size", "0", "--include-active"];

#[test]
fn scan_reports_candidates_and_deletes_nothing() {
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["scan", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("npm cache"))
        .stdout(predicate::str::contains("node_modules"))
        .stdout(predicate::str::contains("Total reclaimable"));

    assert!(
        sandbox.path(".npm/_cacache/blob").exists(),
        "scan must never delete"
    );
}

#[test]
fn scan_json_is_valid_and_carries_the_decision_signals() {
    let sandbox = Sandbox::populated();

    let output = sandbox
        .cmd()
        .args(["scan", "--all", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let candidates = json["candidates"].as_array().expect("candidates array");
    assert!(!candidates.is_empty());

    // The evidence a script would need to make its own decision.
    let first = &candidates[0];
    for key in [
        "id", "provider", "group", "label", "tier", "paths", "size", "signals", "score",
    ] {
        assert!(!first[key].is_null(), "missing `{key}` in {first}");
    }
    assert!(json["total_reclaimable"].as_u64().unwrap() > 0);
}

#[test]
fn scan_on_an_empty_home_succeeds_and_says_so() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("scan")
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing to reclaim"));
}

#[test]
fn a_dry_run_reports_but_removes_nothing() {
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would remove"))
        .stdout(predicate::str::contains("dry run"));

    assert!(sandbox.path(".npm/_cacache/blob").exists());
    assert!(sandbox.path("dev/app/node_modules").exists());
}

#[test]
fn clean_without_yes_is_a_dry_run_when_not_interactive() {
    // Piped or in CI there is nobody to answer a prompt, so the run must report
    // rather than assume consent.
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run"));

    assert!(
        sandbox.path(".npm/_cacache/blob").exists(),
        "must not delete without consent"
    );
}

#[test]
fn clean_with_yes_removes_and_reports_the_bytes() {
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed"))
        .stdout(predicate::str::contains("freed"));

    assert!(!sandbox.path(".npm/_cacache").exists());
    assert!(!sandbox.path("dev/app/node_modules").exists());
    // The project itself must survive; only its artifacts go.
    assert!(sandbox.path("dev/app/package.json").exists());
}

#[test]
fn a_group_filter_restricts_what_is_touched() {
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["clean", "--group", "node"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert()
        .success();

    assert!(!sandbox.path(".npm/_cacache").exists(), "node was selected");
    assert!(
        sandbox.path(".gradle/caches").exists(),
        "jvm was not selected"
    );
}

#[test]
fn recently_used_items_are_protected_from_cleaning_by_default() {
    // The fixtures are seconds old. Without --include-active, a clean must
    // decline to touch them even though a scan happily lists them.
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args([
            "clean",
            "--min-size",
            "0",
            "--older-than",
            "0",
            "--yes",
            "--purge",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing matches"));

    assert!(sandbox.path(".npm/_cacache/blob").exists());
}

#[test]
fn a_run_is_journalled_and_visible_in_history() {
    let sandbox = Sandbox::populated();

    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert()
        .success();

    sandbox
        .cmd()
        .arg("history")
        .assert()
        .success()
        .stdout(predicate::str::contains("cli"))
        .stdout(predicate::str::contains("FREED"));
}

#[test]
fn history_is_machine_readable() {
    let sandbox = Sandbox::populated();
    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert()
        .success();

    let output = sandbox.cmd().args(["history", "--json"]).output().unwrap();
    let runs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!runs.as_array().unwrap().is_empty());
}

#[test]
fn config_init_writes_a_file_that_validates() {
    let sandbox = Sandbox::new();

    sandbox.cmd().args(["config", "init"]).assert().success();
    assert!(sandbox.path(".config/reclaim/config.toml").is_file());

    sandbox
        .cmd()
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));

    sandbox.cmd().args(["config", "show"]).assert().success();
    sandbox.cmd().args(["config", "path"]).assert().success();
}

#[test]
fn config_init_refuses_to_clobber_without_force() {
    let sandbox = Sandbox::new();
    sandbox.cmd().args(["config", "init"]).assert().success();
    sandbox
        .cmd()
        .args(["config", "init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
    sandbox
        .cmd()
        .args(["config", "init", "--force"])
        .assert()
        .success();
}

#[test]
fn a_broken_config_fails_loudly_rather_than_being_ignored() {
    let sandbox = Sandbox::new();
    sandbox.config("[thresholds]\nmin_sise = \"1GB\"\n");

    sandbox
        .cmd()
        .arg("scan")
        .assert()
        .failure()
        .stderr(predicate::str::contains("config"));
}

#[test]
fn an_invalid_size_on_the_command_line_is_rejected_at_the_boundary() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["scan", "--min-size", "enormous"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("min-size"));
}

#[test]
fn a_missing_root_is_a_clear_error() {
    Command::cargo_bin("reclaim")
        .unwrap()
        .args(["--root", "/definitely/not/here", "scan"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}

#[test]
fn providers_lists_every_ecosystem_and_its_status() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("node."))
        .stdout(predicate::str::contains("rust."))
        .stdout(predicate::str::contains("enabled"))
        // ML is off by default, so the listing must show a disabled row too.
        .stdout(predicate::str::contains("disabled"));
}

#[test]
fn a_disabled_provider_produces_no_candidates() {
    let sandbox = Sandbox::populated();
    sandbox.config("[scan]\nproject_roots = [\"~/dev\"]\n[providers]\ndisabled = [\"node\"]\n");

    let output = sandbox
        .cmd()
        .args(["scan", "--all", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let providers: Vec<String> = json["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["provider"].as_str().unwrap().to_string())
        .collect();

    assert!(
        !providers.iter().any(|p| p.starts_with("node.")),
        "got {providers:?}"
    );
    assert!(
        providers.iter().any(|p| p.starts_with("jvm.")),
        "jvm should remain"
    );
}

#[test]
fn protected_paths_from_config_are_refused() {
    let sandbox = Sandbox::new();
    sandbox
        .file(".npm/_cacache/blob", 2_000_000)
        .config("[delete]\nprotected_paths = [\"~/.npm\"]\n");

    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert();

    assert!(
        sandbox.path(".npm/_cacache/blob").exists(),
        "a configured protected path must never be removed"
    );
}

#[test]
fn a_sandboxed_run_never_executes_machine_wide_commands() {
    // `--root` must scope everything. Shell-action providers act on global state
    // (brew, docker, simctl) that no sandbox can represent, so they are dropped.
    let sandbox = Sandbox::populated();

    let output = sandbox
        .cmd()
        .args(["scan", "--all", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    for candidate in json["candidates"].as_array().unwrap() {
        let action = &candidate["action"]["type"];
        assert_ne!(
            action.as_str(),
            Some("shell"),
            "sandboxed run offered a machine-wide command: {candidate}"
        );
    }
}

#[test]
fn the_schedule_refuses_a_caution_tier_background_job() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["schedule", "install", "--tier", "caution"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("caution"));
}

#[test]
fn schedule_status_on_a_clean_machine_reports_nothing_installed() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["schedule", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No scheduled cleanup"));
}

#[test]
fn help_and_version_work() {
    for args in [vec!["--help"], vec!["--version"], vec!["clean", "--help"]] {
        Command::cargo_bin("reclaim")
            .unwrap()
            .args(&args)
            .assert()
            .success();
    }
}

#[test]
fn piped_output_falls_back_to_a_plain_scan() {
    // Bare `reclaim` is the TUI on a terminal, but `reclaim | grep` must produce
    // a report rather than trying to draw.
    let sandbox = Sandbox::populated();
    sandbox.cmd().assert().success().stdout(
        predicate::str::contains("reclaimable").or(predicate::str::contains("Nothing to reclaim")),
    );
}

/// A hardlinked store must be counted once, so the reported total is the space
/// actually available rather than the sum of each directory.
#[test]
fn hardlinked_content_is_not_double_counted_in_the_total() {
    let sandbox = Sandbox::new();
    sandbox.file(".npm/_cacache/blob", 4_000_000);

    let linked = sandbox.path("dev/app/node_modules/pkg/blob");
    std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
    std::fs::hard_link(sandbox.path(".npm/_cacache/blob"), &linked).unwrap();
    sandbox
        .file("dev/app/package.json", 32)
        .file("dev/app/package-lock.json", 32)
        .config("[scan]\nproject_roots = [\"~/dev\"]\n");

    let output = sandbox
        .cmd()
        .args(["scan", "--all", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let total = json["total_reclaimable"].as_u64().unwrap();

    assert!(
        total < 8_000_000,
        "hardlinked bytes counted twice: total was {total}"
    );
    let shared: u64 = json["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["size"]["shared"].as_u64().unwrap_or(0))
        .sum();
    assert!(shared > 0, "the duplicate bytes must be reported as shared");
}

/// Deleting must never escape the sandbox, whatever the config says.
#[test]
fn nothing_outside_the_root_is_ever_touched() {
    let outside = TempDir::new().unwrap();
    let victim = outside.path().join("precious/data.bin");
    std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
    std::fs::write(&victim, vec![b'x'; 1_000_000]).unwrap();

    let sandbox = Sandbox::populated();
    sandbox
        .cmd()
        .args(["clean"])
        .args(PERMISSIVE)
        .args(["--older-than", "0", "--yes", "--purge"])
        .assert()
        .success();

    assert!(victim.exists(), "a file outside --root must be untouched");
    assert_eq!(std::fs::metadata(&victim).unwrap().len(), 1_000_000);
}
