//! Core engine for `reclaim`: discovery, parallel measurement, staleness scoring
//! and safe deletion of developer disk clutter.
//!
//! The pipeline is a chain of transforms over immutable [`model::Candidate`] values:
//!
//! ```text
//!   project walk  ->  discover  ->  measure  ->  score  ->  filter  ->  execute
//!   (one walk,        (providers,   (parallel,   (pure)     (pure)      (guarded)
//!    shared)           parallel)     sized)
//! ```
//!
//! Nothing in this crate deletes anything without first clearing
//! [`safety::PathGuard`], and that check runs again immediately before the
//! destructive call rather than only at scan time.

pub mod config;
pub mod discovery;
pub mod error;
pub mod exec;
pub mod format;
pub mod journal;
pub mod model;
pub mod pipeline;
pub mod platform;
pub mod safety;
pub mod staleness;
pub mod walk;

pub use config::{Config, DeleteMode};
pub use error::{Error, Result};
pub use exec::{clean, CleanEvent, CleanOptions};
pub use journal::{Disposition, ItemOutcome, Journal, RunRecord, Trigger};
pub use model::{
    Action, Candidate, CandidateBuilder, CandidateId, Group, Kind, ProjectRoot, Regen, Severity,
    Signals, Size, Tier, Warning,
};
pub use pipeline::{scan, Provider, ScanContext, ScanEvent, ScanResult};
pub use platform::Paths;
pub use safety::PathGuard;
pub use staleness::Filter;

/// Version string reported by `--version` and stamped into journal records.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
