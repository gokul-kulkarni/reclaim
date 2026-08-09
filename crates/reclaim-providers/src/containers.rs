//! Containers: Docker, Podman, Colima and Lima.
//!
//! Docker is reclaimed by asking the daemon rather than by deleting paths: its
//! storage is a single opaque disk image on macOS, and removing files under it
//! corrupts the whole installation. Everything here is therefore a shell action.

use reclaim_core::model::{Action, Candidate, CandidateBuilder, Group, Regen, Tier, Warning};
use reclaim_core::pipeline::{Provider, ScanContext};

use crate::support::*;

pub struct Docker;

impl Provider for Docker {
    fn id(&self) -> &'static str {
        "containers.docker"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        if !has_command("docker") || !daemon_running("docker") {
            return Vec::new();
        }

        let mut out = Vec::new();

        // Build cache: pure derived data, the safest thing Docker holds and often
        // tens of gigabytes on a machine that builds images regularly.
        out.push(
            CandidateBuilder::new(
                "containers.docker-buildcache",
                Group::Containers,
                "Docker build cache",
            )
            .path(ctx.paths.home_join(".docker"))
            .detail("BuildKit layer cache. Rebuilt on the next image build.")
            .action(Action::Shell {
                program: "docker".into(),
                args: vec!["builder".into(), "prune".into(), "-a".into(), "-f".into()],
            })
            .tier(Tier::Safe)
            .regen(Regen::Rebuild { minutes: 8 })
            .warn(Warning::info(
                "Only affects cache layers; images and containers are untouched.",
            ))
            .build(),
        );

        // Dangling images: unreferenced by any tag, so nothing can be using them.
        out.push(
            CandidateBuilder::new(
                "containers.docker-dangling",
                Group::Containers,
                "Dangling Docker images",
            )
            .path(ctx.paths.home_join(".docker"))
            .detail("Untagged image layers left behind by rebuilds.")
            .action(Action::Shell {
                program: "docker".into(),
                args: vec!["image".into(), "prune".into(), "-f".into()],
            })
            .tier(Tier::Safe)
            .regen(redownload("the image registry"))
            .build(),
        );

        // All unused images: includes tagged images nothing is currently running,
        // which is a much bigger and more surprising set.
        out.push(
            CandidateBuilder::new("containers.docker-unused", Group::Containers, "All unused Docker images")
                .path(ctx.paths.home_join(".docker"))
                .detail("Every image not used by a running container, including tagged ones you pulled deliberately.")
                .action(Action::Shell {
                    program: "docker".into(),
                    args: vec!["system".into(), "prune".into(), "-a".into(), "-f".into()],
                })
                .tier(Tier::Review)
                .regen(redownload("the image registry"))
                .warn(Warning::caution(
                    "Removes tagged images too. Anything not pullable from a registry \
                     (locally built, never pushed) is gone.",
                ))
                .build(),
        );

        // Volumes are opt-in: they hold databases and other state.
        if ctx.config.providers.containers.include_volumes {
            out.push(
                CandidateBuilder::new("containers.docker-volumes", Group::Containers, "Unused Docker volumes")
                    .path(ctx.paths.home_join(".docker"))
                    .detail("Named volumes not attached to any container.")
                    .action(Action::Shell {
                        program: "docker".into(),
                        args: vec!["volume".into(), "prune".into(), "-a".into(), "-f".into()],
                    })
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(Warning::danger(
                        "Volumes hold database contents and other persistent state. This is unrecoverable.",
                    ))
                    .build(),
            );
        }

        out
    }
}

pub struct ContainerVms;

impl Provider for ContainerVms {
    fn id(&self) -> &'static str {
        "containers.vms"
    }

    fn discover(&self, ctx: &ScanContext) -> Vec<Candidate> {
        let p = &ctx.paths;
        let mut out = Vec::new();

        // The Docker Desktop disk image grows and never shrinks on its own, but
        // deleting it destroys every image, container and volume at once.
        for (label, rel) in [
            (
                "Docker Desktop disk image",
                "Library/Containers/com.docker.docker/Data/vms",
            ),
            (
                "Docker Desktop data",
                "Library/Containers/com.docker.docker/Data/log",
            ),
        ] {
            let path = p.home_join(rel);
            if !path.exists() {
                continue;
            }
            let is_disk = rel.ends_with("vms");
            out.push(
                CandidateBuilder::new("containers.docker-vm", Group::Containers, label)
                    .path(path)
                    .tier(if is_disk { Tier::Caution } else { Tier::Safe })
                    .regen(if is_disk {
                        Regen::Never
                    } else {
                        Regen::Automatic {
                            on: "next start".into(),
                        }
                    })
                    .warn_if(
                        is_disk,
                        Warning::danger(
                            "This single file contains every Docker image, container and volume. \
                             Deleting it is equivalent to a factory reset of Docker.",
                        ),
                    )
                    .build(),
            );
        }

        for (id, label, rel) in [
            ("containers.colima", "Colima VM data", ".colima"),
            ("containers.lima", "Lima VM data", ".lima"),
            (
                "containers.podman",
                "Podman machine data",
                ".local/share/containers",
            ),
        ] {
            let path = p.home_join(rel);
            if !path.exists() {
                continue;
            }
            out.push(
                CandidateBuilder::new(id, Group::Containers, label)
                    .path(path)
                    .tier(Tier::Caution)
                    .regen(Regen::Never)
                    .warn(Warning::danger(
                        "Contains the virtual machine disk with all of its images and volumes.",
                    ))
                    .build(),
            );
        }

        out
    }
}

/// Whether a container daemon is reachable. Offering a prune that immediately
/// fails with "cannot connect" is worse than offering nothing.
fn daemon_running(program: &str) -> bool {
    std::process::Command::new(program)
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn providers() -> Vec<Box<dyn Provider>> {
    vec![Box::new(Docker), Box::new(ContainerVms)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestHome;

    #[test]
    fn docker_candidates_all_use_shell_actions() {
        // Deleting files under Docker's storage corrupts the installation; every
        // reclaim must go through the daemon.
        let home = TestHome::new();
        for candidate in home.discover(&Docker) {
            assert!(
                candidate.action.is_shell(),
                "{} must reclaim via the daemon, not by deleting paths",
                candidate.provider
            );
        }
    }

    #[test]
    fn volumes_are_withheld_unless_explicitly_enabled() {
        let home = TestHome::new();
        let found = home.discover(&Docker);
        assert!(!found
            .iter()
            .any(|c| c.provider == "containers.docker-volumes"));
    }

    #[test]
    fn volumes_when_enabled_are_marked_unrecoverable() {
        let mut home = TestHome::new();
        home.config.providers.containers.include_volumes = true;
        let found = home.discover(&Docker);

        // Only present when a docker daemon is actually reachable.
        if let Some(volumes) = found
            .iter()
            .find(|c| c.provider == "containers.docker-volumes")
        {
            assert_eq!(volumes.tier, Tier::Caution);
            assert_eq!(volumes.regen, Regen::Never);
            assert!(volumes
                .warnings
                .iter()
                .any(|w| w.severity == reclaim_core::Severity::Danger));
        }
    }

    #[test]
    fn the_docker_desktop_disk_image_is_flagged_as_a_factory_reset() {
        let home = TestHome::new();
        home.file(
            "Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw",
            4096,
        );

        let found = home.discover(&ContainerVms);
        let vm = found
            .iter()
            .find(|c| c.label.contains("disk image"))
            .expect("vm disk");
        assert_eq!(vm.tier, Tier::Caution);
        assert!(vm
            .warnings
            .iter()
            .any(|w| w.message.contains("factory reset")));
    }

    #[test]
    fn nothing_is_offered_without_any_container_runtime() {
        let home = TestHome::new();
        assert!(home.discover(&ContainerVms).is_empty());
    }
}
