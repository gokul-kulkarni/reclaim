//! Deriving freshness signals and the reclaim score.
//!
//! The score is only a sorting aid. The argument for deleting something is the
//! evidence in [`Candidate::warnings`] plus the raw signals; the score exists so
//! that the obvious wins float to the top of a 200-row list.
//!
//! Everything here is a pure function of its inputs, which is what makes the
//! ranking behaviour testable without touching a filesystem.

use std::path::Path;
use std::time::SystemTime;

use crate::config::Config;
use crate::model::{days_since, Candidate, Kind, Signals, Size};
use crate::walk::{self, Measurement};

/// Anything touched inside this window is treated as possibly in use right now.
const ACTIVE_WINDOW_HOURS: u64 = 24;

/// Upper bound on the staleness multiplier. Beyond roughly three threshold
/// periods, "even older" stops being useful information for ranking.
const MAX_STALENESS: f64 = 3.0;

/// Build the freshness signals for a candidate from its measurement.
///
/// For a [`Kind::ProjectArtifact`] this reads the owning project's source files
/// and git activity, which is the whole point: a `target/` directory whose own
/// mtime is six months old still belongs to a repository you committed to
/// yesterday, and that project is not stale.
pub fn derive_signals(
    candidate: &Candidate,
    measurement: &Measurement,
    max_depth: usize,
) -> Signals {
    let artifact_mtime = measurement.newest_mtime.unwrap_or(SystemTime::UNIX_EPOCH);

    let (source_mtime, vcs_activity) = match (candidate.kind, candidate.project.as_deref()) {
        (Kind::ProjectArtifact, Some(project)) => (
            walk::newest_source_mtime(project, max_depth),
            walk::vcs_activity(project),
        ),
        _ => (None, None),
    };

    let signals = Signals {
        artifact_mtime,
        artifact_atime: measurement.newest_atime,
        source_mtime,
        vcs_activity,
        last_used_days: 0,
        active_now: false,
    };

    let last_used = signals.last_used();
    Signals {
        last_used_days: days_since(last_used),
        active_now: is_active_now(last_used),
        ..signals
    }
}

fn is_active_now(last_used: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(last_used)
        .map(|d| d.as_secs() < ACTIVE_WINDOW_HOURS * 3600)
        .unwrap_or(true) // a future timestamp means clock skew; treat as active
}

/// How stale something is, relative to the configured baseline. 0.0..=3.0.
pub fn staleness_factor(last_used_days: u32, stale_after_days: u32) -> f64 {
    if stale_after_days == 0 {
        return MAX_STALENESS;
    }
    (f64::from(last_used_days) / f64::from(stale_after_days)).clamp(0.0, MAX_STALENESS)
}

/// The reclaim score, 0.0..=100.0. Higher means "more clearly worth deleting".
///
///   score = sqrt(GB) x staleness x tier_weight / regen_weight
///
/// The square root on size is deliberate: a 40 GB cache is worth more attention
/// than a 4 GB one, but not ten times more, and without the dampening a single
/// enormous but actively-used directory would dominate the whole list.
pub fn score(candidate: &Candidate, config: &Config) -> f64 {
    let Some(size) = candidate.size else {
        return 0.0;
    };
    let Some(signals) = candidate.signals.as_ref() else {
        return 0.0;
    };

    if size.on_disk == 0 {
        return 0.0;
    }

    let gigabytes = size.on_disk as f64 / (1024.0 * 1024.0 * 1024.0);
    let size_factor = gigabytes.sqrt();
    let stale = staleness_factor(signals.last_used_days, config.thresholds.stale_after_days);

    let raw = size_factor * stale * candidate.tier.weight() / candidate.regen.weight();

    // Something written in the last day is very likely a build in flight. Rank it
    // to the bottom rather than hiding it: the user may still want to know it exists.
    let raw = if signals.active_now { raw * 0.1 } else { raw };

    // Mostly-hardlinked stores free far less than their apparent size and break
    // other installs, so discount by the fraction that is shared.
    let raw = raw * (1.0 - size.shared_ratio() * 0.75);

    // Map to 0..100 with a saturating curve, so the top of the list is readable
    // rather than a handful of items at 100 and everything else near zero.
    (100.0 * (raw / (raw + 4.0))).clamp(0.0, 100.0)
}

/// Apply scores to a set of candidates, returning a new sorted vector.
///
/// Sorted by score descending, then by size descending so that ties between
/// equally-stale items put the bigger win first.
pub fn rank(candidates: Vec<Candidate>, config: &Config) -> Vec<Candidate> {
    let mut scored: Vec<Candidate> = candidates
        .into_iter()
        .map(|c| {
            let s = score(&c, config);
            c.with_score(s)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.reclaimable().cmp(&a.reclaimable()))
            .then_with(|| a.id.cmp(&b.id))
    });
    scored
}

/// Filters applied to a measured, scored candidate list.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub min_size: u64,
    pub max_tier: Option<crate::model::Tier>,
    pub min_age_days: Option<u32>,
    pub groups: Option<Vec<crate::model::Group>>,
    pub providers: Option<Vec<String>>,
    /// Include items touched in the last 24h. Off by default: deleting a cache a
    /// running build is writing to produces confusing failures.
    pub include_active: bool,
}

impl Filter {
    pub fn matches(&self, candidate: &Candidate) -> bool {
        if candidate.reclaimable() < self.min_size {
            return false;
        }
        if let Some(max) = self.max_tier {
            if candidate.tier > max {
                return false;
            }
        }
        if let Some(min_age) = self.min_age_days {
            if candidate.last_used_days().unwrap_or(0) < min_age {
                return false;
            }
        }
        if let Some(groups) = &self.groups {
            if !groups.contains(&candidate.group) {
                return false;
            }
        }
        if let Some(providers) = &self.providers {
            let matched = providers.iter().any(|p| {
                candidate.provider == *p
                    || candidate
                        .provider
                        .strip_prefix(p)
                        .is_some_and(|r| r.starts_with('.'))
            });
            if !matched {
                return false;
            }
        }
        if !self.include_active {
            if let Some(signals) = &candidate.signals {
                if signals.active_now {
                    return false;
                }
            }
        }
        true
    }

    pub fn apply(&self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        candidates.into_iter().filter(|c| self.matches(c)).collect()
    }
}

/// Total reclaimable bytes across a candidate list.
pub fn total_reclaimable(candidates: &[Candidate]) -> u64 {
    candidates.iter().map(Candidate::reclaimable).sum()
}

/// Totals per group, for the summary line and the treemap's first level.
pub fn totals_by_group(candidates: &[Candidate]) -> Vec<(crate::model::Group, Size)> {
    let mut totals: std::collections::BTreeMap<crate::model::Group, Size> = Default::default();
    for c in candidates {
        if let Some(size) = c.size {
            let entry = totals.entry(c.group).or_default();
            *entry = *entry + size;
        }
    }
    let mut out: Vec<_> = totals.into_iter().collect();
    out.sort_by_key(|(_, size)| std::cmp::Reverse(size.on_disk));
    out
}

/// A project's own freshness, used when deciding whether to offer its artifacts.
pub fn project_last_activity(project: &Path, max_depth: usize) -> Option<SystemTime> {
    let source = walk::newest_source_mtime(project, max_depth);
    let vcs = walk::vcs_activity(project);
    source.max(vcs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CandidateBuilder, Group, Regen, Tier};
    use rstest::rstest;
    use std::time::Duration;

    fn candidate(tier: Tier, on_disk: u64, days: u32) -> Candidate {
        let c = CandidateBuilder::new("test.thing", Group::Node, "thing")
            .path("/home/tester/.cache/thing")
            .tier(tier)
            .build();
        let size = Size {
            on_disk,
            logical: on_disk,
            ..Size::default()
        };
        let signals = Signals {
            artifact_mtime: SystemTime::now() - Duration::from_secs(u64::from(days) * 86_400),
            artifact_atime: None,
            source_mtime: None,
            vcs_activity: None,
            last_used_days: days,
            active_now: days == 0,
        };
        c.with_measurement(size, signals)
    }

    const GB: u64 = 1024 * 1024 * 1024;

    #[rstest]
    #[case(0, 60, 0.0)]
    #[case(30, 60, 0.5)]
    #[case(60, 60, 1.0)]
    #[case(180, 60, 3.0)]
    #[case(3650, 60, 3.0)]
    fn staleness_scales_then_saturates(
        #[case] days: u32,
        #[case] threshold: u32,
        #[case] expected: f64,
    ) {
        assert!((staleness_factor(days, threshold) - expected).abs() < 1e-9);
    }

    #[test]
    fn unmeasured_candidates_score_zero() {
        let c = CandidateBuilder::new("test.thing", Group::Node, "thing")
            .path("/x")
            .build();
        assert_eq!(score(&c, &Config::default()), 0.0);
    }

    #[test]
    fn a_bigger_older_cache_outranks_a_smaller_newer_one() {
        let cfg = Config::default();
        let big_old = score(&candidate(Tier::Safe, 20 * GB, 200), &cfg);
        let small_new = score(&candidate(Tier::Safe, 1 * GB, 20), &cfg);
        assert!(big_old > small_new, "{big_old} should beat {small_new}");
    }

    #[test]
    fn risk_tier_lowers_the_score_at_equal_size_and_age() {
        let cfg = Config::default();
        let safe = score(&candidate(Tier::Safe, 5 * GB, 120), &cfg);
        let review = score(&candidate(Tier::Review, 5 * GB, 120), &cfg);
        let caution = score(&candidate(Tier::Caution, 5 * GB, 120), &cfg);
        assert!(safe > review, "safe {safe} > review {review}");
        assert!(review > caution, "review {review} > caution {caution}");
    }

    #[test]
    fn an_active_project_artifact_ranks_below_a_dormant_one() {
        // This is the behaviour the whole scoring model exists for.
        let cfg = Config::default();
        let dormant = score(&candidate(Tier::Safe, 3 * GB, 300), &cfg);
        let active = score(&candidate(Tier::Safe, 3 * GB, 0), &cfg);
        assert!(
            dormant > active * 5.0,
            "dormant {dormant} must dominate active {active}"
        );
    }

    #[test]
    fn a_mostly_hardlinked_store_is_discounted() {
        let cfg = Config::default();
        let mut shared = candidate(Tier::Safe, 4 * GB, 120);
        let solo_score = score(&shared, &cfg);

        shared.size = Some(Size {
            on_disk: 4 * GB,
            shared: 12 * GB,
            ..Size::default()
        });
        let shared_score = score(&shared, &cfg);
        assert!(
            shared_score < solo_score,
            "a store that is 75% hardlinked elsewhere must rank lower: {shared_score} vs {solo_score}"
        );
    }

    #[test]
    fn expensive_regeneration_lowers_the_score() {
        let cfg = Config::default();
        let auto = candidate(Tier::Safe, 5 * GB, 120);
        let mut never = auto.clone();
        never.regen = Regen::Never;
        assert!(score(&auto, &cfg) > score(&never, &cfg));
    }

    #[test]
    fn scores_stay_within_bounds_even_for_absurd_inputs() {
        let cfg = Config::default();
        let huge = candidate(Tier::Safe, 900 * GB, 9999);
        let s = score(&huge, &cfg);
        assert!((0.0..=100.0).contains(&s), "score out of range: {s}");
    }

    #[test]
    fn ranking_sorts_by_score_then_size() {
        let cfg = Config::default();
        let ranked = rank(
            vec![
                candidate(Tier::Caution, 1 * GB, 10),
                candidate(Tier::Safe, 20 * GB, 300),
                candidate(Tier::Review, 5 * GB, 100),
            ],
            &cfg,
        );
        let scores: Vec<f64> = ranked.iter().map(|c| c.score.unwrap()).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "not descending: {scores:?}"
        );
        assert_eq!(ranked[0].tier, Tier::Safe);
    }

    #[test]
    fn filter_hides_items_below_the_size_floor() {
        let f = Filter {
            min_size: 100 * 1024 * 1024,
            ..Default::default()
        };
        assert!(!f.matches(&candidate(Tier::Safe, 1024, 100)));
        assert!(f.matches(&candidate(Tier::Safe, GB, 100)));
    }

    #[test]
    fn filter_excludes_actively_used_items_by_default() {
        let active = candidate(Tier::Safe, GB, 0);
        assert!(!Filter {
            min_size: 0,
            ..Default::default()
        }
        .matches(&active));
        let permissive = Filter {
            min_size: 0,
            include_active: true,
            ..Default::default()
        };
        assert!(permissive.matches(&active));
    }

    #[test]
    fn filter_respects_the_tier_ceiling() {
        let f = Filter {
            min_size: 0,
            max_tier: Some(Tier::Safe),
            ..Default::default()
        };
        assert!(f.matches(&candidate(Tier::Safe, GB, 100)));
        assert!(!f.matches(&candidate(Tier::Review, GB, 100)));
        assert!(!f.matches(&candidate(Tier::Caution, GB, 100)));
    }

    #[test]
    fn filter_respects_a_minimum_age() {
        let f = Filter {
            min_size: 0,
            min_age_days: Some(90),
            ..Default::default()
        };
        assert!(!f.matches(&candidate(Tier::Safe, GB, 30)));
        assert!(f.matches(&candidate(Tier::Safe, GB, 120)));
    }

    #[test]
    fn filter_matches_providers_by_id_and_group_prefix() {
        let c = candidate(Tier::Safe, GB, 100);
        let by_prefix = Filter {
            min_size: 0,
            providers: Some(vec!["test".into()]),
            ..Default::default()
        };
        let by_id = Filter {
            min_size: 0,
            providers: Some(vec!["test.thing".into()]),
            ..Default::default()
        };
        let other = Filter {
            min_size: 0,
            providers: Some(vec!["rust".into()]),
            ..Default::default()
        };
        assert!(by_prefix.matches(&c));
        assert!(by_id.matches(&c));
        assert!(!other.matches(&c));
    }

    #[test]
    fn totals_by_group_are_sorted_by_size() {
        let mut node = candidate(Tier::Safe, 5 * GB, 100);
        node.group = Group::Node;
        let mut rust = candidate(Tier::Safe, 20 * GB, 100);
        rust.group = Group::Rust;
        let totals = totals_by_group(&[node, rust]);
        assert_eq!(totals[0].0, Group::Rust);
        assert_eq!(totals[0].1.on_disk, 20 * GB);
    }

    #[test]
    fn total_reclaimable_ignores_unmeasured_candidates() {
        let measured = candidate(Tier::Safe, GB, 100);
        let unmeasured = CandidateBuilder::new("test.other", Group::Node, "other")
            .path("/y")
            .build();
        assert_eq!(total_reclaimable(&[measured, unmeasured]), GB);
    }
}
