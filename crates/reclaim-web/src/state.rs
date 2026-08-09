//! Server state and the auth token.

use std::sync::{Arc, RwLock};

use reclaim_core::config::Config;
use reclaim_core::journal::Journal;
use reclaim_core::pipeline::{Provider, ScanResult};
use reclaim_core::{PathGuard, Paths};

/// A random per-process token. Every API request must present it.
///
/// This is not defence against a determined local attacker — anything running as
/// the user could read the process arguments. It is there to stop the ordinary
/// failure mode: a web page the user has open in another tab discovering a local
/// port that will delete their files if asked nicely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison, so a token cannot be recovered byte by byte
    /// from response timings.
    pub fn matches(&self, candidate: &str) -> bool {
        if candidate.len() != self.0.len() {
            return false;
        }
        self.0
            .bytes()
            .zip(candidate.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

/// Shared state for the axum handlers.
#[derive(Clone)]
pub struct ServerState {
    pub paths: Paths,
    pub config: Config,
    pub token: Token,
    pub guard: Arc<PathGuard>,
    pub journal: Arc<Journal>,
    pub providers: Arc<Vec<Box<dyn Provider>>>,
    /// The most recent scan, so `POST /api/clean` acts on measured candidates
    /// rather than trusting sizes supplied by the client.
    pub last_scan: Arc<RwLock<Option<ScanResult>>>,
    pub dev: bool,
}

impl ServerState {
    pub fn new(paths: Paths, config: Config, dev: bool) -> Self {
        let guard = PathGuard::new(&paths).protect(config.resolved_protected_paths(&paths));
        let journal = Journal::new(paths.journal_dir());

        Self {
            token: Token::generate(),
            guard: Arc::new(guard),
            journal: Arc::new(journal),
            providers: Arc::new(reclaim_providers::all()),
            last_scan: Arc::new(RwLock::new(None)),
            paths,
            config,
            dev,
        }
    }

    pub fn store_scan(&self, result: ScanResult) {
        if let Ok(mut slot) = self.last_scan.write() {
            *slot = Some(result);
        }
    }

    /// Look up candidates by id from the last scan.
    ///
    /// The client sends ids, never paths or sizes: accepting a path from the
    /// browser would let any request that clears auth name an arbitrary target.
    pub fn candidates_by_id(&self, ids: &[String]) -> Vec<reclaim_core::Candidate> {
        let Ok(slot) = self.last_scan.read() else {
            return Vec::new();
        };
        let Some(result) = slot.as_ref() else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| result.all.iter().find(|c| c.id.0 == *id))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_per_process() {
        assert_ne!(Token::generate(), Token::generate());
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_value() {
        let token = Token::generate();
        assert!(token.matches(token.as_str()));
        assert!(!token.matches("wrong"));
        assert!(!token.matches(""));

        // A prefix must not pass, even though it matches byte for byte so far.
        let prefix = &token.as_str()[..8];
        assert!(!token.matches(prefix));
    }

    #[test]
    fn tokens_are_long_enough_not_to_be_guessed() {
        assert!(Token::generate().as_str().len() >= 32);
    }

    fn state() -> ServerState {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        std::mem::forget(tmp); // the test only needs the path to exist for its lifetime
        ServerState::new(Paths::with_home(home), Config::default(), false)
    }

    #[test]
    fn unknown_candidate_ids_resolve_to_nothing() {
        let state = state();
        assert!(state.candidates_by_id(&["not-a-real-id".into()]).is_empty());
    }

    #[test]
    fn candidate_lookup_returns_nothing_before_a_scan_has_run() {
        // A clean request that arrives before any scan must be a no-op, not a
        // chance for the client to name its own targets.
        let state = state();
        assert!(state.candidates_by_id(&["anything".into()]).is_empty());
    }
}
