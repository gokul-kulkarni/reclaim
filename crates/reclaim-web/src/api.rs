//! HTTP routes.

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use reclaim_core::exec::{self, CleanOptions};
use reclaim_core::journal::Trigger;
use reclaim_core::model::{humanize_age, Candidate};
use reclaim_core::pipeline;

use crate::assets;
use crate::state::ServerState;

pub fn router(state: ServerState) -> Router {
    let api = Router::new()
        .route("/scan", get(scan))
        .route("/candidates", get(candidates))
        .route("/clean", post(clean))
        .route("/history", get(history))
        .route("/config", get(config))
        .route("/providers", get(providers))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));

    Router::new()
        .nest("/api", api)
        .fallback(assets::serve)
        .with_state(state)
}

/// Reject anything that does not present the token, or that comes from a
/// non-loopback origin.
async fn authenticate(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // A cross-site request from a page the user has open would carry that page's
    // Origin. Requests the UI itself makes carry a loopback origin or none at all.
    if let Some(origin) = request.headers().get(axum::http::header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        let loopback = origin.starts_with("http://127.0.0.1:")
            || origin.starts_with("http://localhost:")
            || origin.starts_with("http://[::1]:");
        if !loopback {
            return Err(ApiError::forbidden(
                "requests must originate from localhost",
            ));
        }
    }

    if !token_present(&state, request.headers(), request.uri().query()) {
        return Err(ApiError::unauthorized());
    }

    Ok(next.run(request).await)
}

fn token_present(state: &ServerState, headers: &HeaderMap, query: Option<&str>) -> bool {
    if let Some(header) = headers.get("x-reclaim-token").and_then(|v| v.to_str().ok()) {
        if state.token.matches(header) {
            return true;
        }
    }
    // The query parameter exists for EventSource, which cannot set headers.
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.strip_prefix("t="))
        .any(|value| state.token.matches(value))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ScanResponse {
    total_reclaimable: u64,
    projects_scanned: usize,
    elapsed_ms: u64,
    unreadable: usize,
    hidden_count: usize,
    hidden_bytes: u64,
    groups: Vec<GroupTotal>,
    candidates: Vec<CandidateView>,
}

#[derive(Debug, Serialize)]
struct GroupTotal {
    group: String,
    title: String,
    on_disk: u64,
}

/// The candidate shape the frontend consumes.
///
/// A view type rather than the raw model: it carries the already-humanised
/// strings so the browser never has to re-derive them and drift from the CLI.
#[derive(Debug, Serialize)]
struct CandidateView {
    id: String,
    provider: String,
    group: String,
    group_title: String,
    label: String,
    detail: String,
    paths: Vec<String>,
    tier: String,
    kind: String,
    on_disk: u64,
    shared: u64,
    files: u64,
    partial: bool,
    last_used_days: Option<u32>,
    last_used_human: String,
    active_now: bool,
    score: f64,
    regen: String,
    warnings: Vec<WarningView>,
}

#[derive(Debug, Serialize)]
struct WarningView {
    severity: String,
    message: String,
}

impl CandidateView {
    fn from(candidate: &Candidate, paths: &reclaim_core::Paths) -> Self {
        let size = candidate.size.unwrap_or_default();
        Self {
            id: candidate.id.0.clone(),
            provider: candidate.provider.clone(),
            group: candidate.group.as_str().to_string(),
            group_title: candidate.group.title().to_string(),
            label: candidate.label.clone(),
            detail: candidate.detail.clone(),
            paths: candidate.paths.iter().map(|p| paths.contract(p)).collect(),
            tier: candidate.tier.as_str().to_string(),
            kind: format!("{:?}", candidate.kind).to_lowercase(),
            on_disk: size.on_disk,
            shared: size.shared,
            files: size.files,
            partial: size.partial,
            last_used_days: candidate.last_used_days(),
            last_used_human: candidate
                .last_used_days()
                .map(humanize_age)
                .unwrap_or_else(|| "unknown".into()),
            active_now: candidate.signals.as_ref().is_some_and(|s| s.active_now),
            score: candidate.score.unwrap_or(0.0),
            regen: candidate.regen.summary(),
            warnings: candidate
                .warnings
                .iter()
                .map(|w| WarningView {
                    severity: format!("{:?}", w.severity).to_lowercase(),
                    message: w.message.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScanQuery {
    /// Ignore the configured size threshold.
    #[serde(default)]
    all: bool,
}

async fn scan(
    State(state): State<ServerState>,
    Query(query): Query<ScanQuery>,
) -> Result<Json<ScanResponse>, ApiError> {
    // Reporting always includes actively-used items: they are ranked near the
    // bottom by scoring and flagged in the UI, but never hidden.
    let filter = reclaim_core::staleness::Filter {
        min_size: if query.all {
            0
        } else {
            pipeline::filter_from_config(&state.config).min_size
        },
        include_active: true,
        ..Default::default()
    };

    // The scan is CPU and IO bound, so it must not run on the async runtime's
    // worker threads or it would stall every other request for its duration.
    let scan_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        pipeline::scan(
            &scan_state.providers,
            &scan_state.paths,
            &scan_state.config,
            &filter,
            None,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("scan failed: {e}")))?;

    let (hidden_count, hidden_bytes) = result.hidden();
    let response = ScanResponse {
        total_reclaimable: result.total_reclaimable(),
        projects_scanned: result.projects_scanned,
        elapsed_ms: result.elapsed_ms,
        unreadable: result.unreadable.len(),
        hidden_count,
        hidden_bytes,
        groups: result
            .by_group()
            .into_iter()
            .map(|(group, size)| GroupTotal {
                group: group.as_str().to_string(),
                title: group.title().to_string(),
                on_disk: size.on_disk,
            })
            .collect(),
        candidates: result
            .candidates
            .iter()
            .map(|c| CandidateView::from(c, &state.paths))
            .collect(),
    };

    state.store_scan(result);
    Ok(Json(response))
}

async fn candidates(
    State(state): State<ServerState>,
) -> Result<Json<Vec<CandidateView>>, ApiError> {
    let slot = state
        .last_scan
        .read()
        .map_err(|_| ApiError::internal("state poisoned"))?;
    let views = slot
        .as_ref()
        .map(|r| {
            r.candidates
                .iter()
                .map(|c| CandidateView::from(c, &state.paths))
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(views))
}

#[derive(Debug, Deserialize)]
struct CleanRequest {
    ids: Vec<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
    /// Explicit acknowledgement for caution-tier items. Without it they are skipped.
    #[serde(default)]
    confirm_caution: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct CleanResponse {
    dry_run: bool,
    bytes_freed: u64,
    bytes_trashed: u64,
    succeeded: bool,
    summary: String,
    skipped_caution: usize,
    items: Vec<CleanItemView>,
}

#[derive(Debug, Serialize)]
struct CleanItemView {
    id: String,
    label: String,
    disposition: String,
    freed_bytes: u64,
    error: Option<String>,
}

async fn clean(
    State(state): State<ServerState>,
    Json(request): Json<CleanRequest>,
) -> Result<Json<CleanResponse>, ApiError> {
    // Ids are resolved against the last scan, so the browser can only ever name
    // something this process already measured and vetted.
    let mut chosen = state.candidates_by_id(&request.ids);

    if chosen.is_empty() {
        return Err(ApiError::bad_request(
            "no matching candidates: run a scan first, then send ids from that scan",
        ));
    }

    let mut skipped_caution = 0;
    if state.config.delete.confirm_caution && !request.confirm_caution {
        let before = chosen.len();
        chosen.retain(|c| c.tier != reclaim_core::Tier::Caution);
        skipped_caution = before - chosen.len();
    }

    if chosen.is_empty() {
        return Ok(Json(CleanResponse {
            dry_run: request.dry_run,
            bytes_freed: 0,
            bytes_trashed: 0,
            succeeded: true,
            summary: "nothing to do: all selected items need explicit confirmation".into(),
            skipped_caution,
            items: Vec::new(),
        }));
    }

    let options = CleanOptions {
        dry_run: request.dry_run,
        mode: state.config.delete.mode,
        trigger: Trigger::Web,
        concurrency: state.config.scan.threads(),
    };

    let exec_state = state.clone();
    let record = tokio::task::spawn_blocking(move || {
        exec::clean(&chosen, &exec_state.guard, &options, None)
    })
    .await
    .map_err(|e| ApiError::internal(format!("clean failed: {e}")))?;

    let _ = state.journal.write(&record);

    Ok(Json(CleanResponse {
        dry_run: record.dry_run,
        bytes_freed: record.bytes_freed(),
        bytes_trashed: record.bytes_trashed(),
        succeeded: record.succeeded(),
        summary: record.summary(),
        skipped_caution,
        items: record
            .items
            .iter()
            .map(|i| CleanItemView {
                id: i.id.0.clone(),
                label: i.label.clone(),
                disposition: format!("{:?}", i.disposition).to_lowercase(),
                freed_bytes: i.freed_bytes,
                error: i.error.clone(),
            })
            .collect(),
    }))
}

async fn history(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let records = state.journal.read_recent(25);
    Json(serde_json::json!({ "runs": records }))
}

async fn config(State(state): State<ServerState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "config": state.config,
        "home": state.paths.home().display().to_string(),
        "config_path": state.paths.config_file().display().to_string(),
        "version": reclaim_core::VERSION,
    }))
}

async fn providers(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let list: Vec<_> = state
        .providers
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id(),
                "enabled": state.config.providers.is_enabled(p.id()),
            })
        })
        .collect();
    Json(serde_json::json!({ "providers": list }))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid token; open the URL printed by `reclaim ui`".into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use reclaim_core::config::Config;
    use reclaim_core::Paths;
    use tower::ServiceExt;

    fn test_state() -> (tempfile::TempDir, ServerState) {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let state = ServerState::new(Paths::with_home(home), Config::default(), false);
        (tmp, state)
    }

    /// A sandbox home with a real cache directory in it, plus a matching config
    /// so the size threshold does not hide the fixture.
    fn populated_state() -> (tempfile::TempDir, ServerState) {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let cache = home.join(".npm/_cacache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("blob"), vec![b'x'; 2_000_000]).unwrap();

        let mut config = Config::default();
        config.thresholds.min_size = "0".into();
        let state = ServerState::new(Paths::with_home(home), config, false);
        (tmp, state)
    }

    fn get(state: ServerState, path: &str) -> HttpRequest<Body> {
        let token = state.token.as_str().to_string();
        HttpRequest::builder()
            .uri(path)
            .header("x-reclaim-token", token)
            .body(Body::empty())
            .unwrap()
    }

    fn post_json(state: &ServerState, path: &str, body: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri(path)
            .header("x-reclaim-token", state.token.as_str())
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json_of(state: ServerState, request: HttpRequest<Body>) -> serde_json::Value {
        let (status, body) = send(state, request).await;
        assert!(status.is_success(), "{status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn send(state: ServerState, request: HttpRequest<Body>) -> (StatusCode, String) {
        let response = router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn api_requests_without_a_token_are_rejected() {
        let (_tmp, state) = test_state();
        let (status, body) = send(
            state,
            HttpRequest::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("token"), "{body}");
    }

    #[tokio::test]
    async fn a_wrong_token_is_rejected() {
        let (_tmp, state) = test_state();
        let (status, _) = send(
            state,
            HttpRequest::builder()
                .uri("/api/config?t=not-the-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_token_works_in_a_header_or_a_query_parameter() {
        let (_tmp, state) = test_state();
        let token = state.token.as_str().to_string();

        let (status, _) = send(
            state.clone(),
            HttpRequest::builder()
                .uri("/api/config")
                .header("x-reclaim-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // EventSource cannot set headers, so the query form must work too.
        let (status, _) = send(
            state,
            HttpRequest::builder()
                .uri(format!("/api/config?t={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_cross_site_origin_is_refused_even_with_a_valid_token() {
        // Defends against a page the user has open in another tab reaching in.
        let (_tmp, state) = test_state();
        let token = state.token.as_str().to_string();

        let (status, body) = send(
            state,
            HttpRequest::builder()
                .uri("/api/config")
                .header("x-reclaim-token", &token)
                .header("origin", "https://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("localhost"), "{body}");
    }

    #[tokio::test]
    async fn a_loopback_origin_is_accepted() {
        let (_tmp, state) = test_state();
        let token = state.token.as_str().to_string();

        let (status, _) = send(
            state,
            HttpRequest::builder()
                .uri("/api/config")
                .header("x-reclaim-token", &token)
                .header("origin", "http://127.0.0.1:7391")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn cleaning_before_a_scan_is_refused_rather_than_guessing() {
        // The client may only name ids the server has already measured.
        let (_tmp, state) = test_state();
        let token = state.token.as_str().to_string();

        let (status, body) = send(
            state,
            HttpRequest::builder()
                .method("POST")
                .uri("/api/clean")
                .header("x-reclaim-token", &token)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ids":["anything"],"dry_run":true}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("run a scan first"), "{body}");
    }

    #[tokio::test]
    async fn a_scan_returns_totals_and_group_breakdown() {
        let (_tmp, state) = test_state();
        let token = state.token.as_str().to_string();

        let (status, body) = send(
            state,
            HttpRequest::builder()
                .uri(format!("/api/scan?t={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(json["total_reclaimable"].is_number());
        assert!(json["groups"].is_array());
        assert!(json["candidates"].is_array());
    }

    #[tokio::test]
    async fn candidates_are_empty_until_a_scan_has_run_then_populated() {
        let (_tmp, state) = populated_state();

        let before = json_of(state.clone(), get(state.clone(), "/api/candidates")).await;
        assert!(before.as_array().unwrap().is_empty());

        json_of(state.clone(), get(state.clone(), "/api/scan?all=true")).await;

        let after = json_of(state.clone(), get(state.clone(), "/api/candidates")).await;
        assert!(
            !after.as_array().unwrap().is_empty(),
            "scan results must be retained"
        );
    }

    #[tokio::test]
    async fn a_dry_run_clean_reports_without_deleting() {
        let (_tmp, state) = populated_state();
        let blob = state.paths.home_join(".npm/_cacache/blob");

        let scan = json_of(state.clone(), get(state.clone(), "/api/scan?all=true")).await;
        let id = scan["candidates"][0]["id"].as_str().unwrap().to_string();

        let body = format!(r#"{{"ids":["{id}"],"dry_run":true}}"#);
        let result = json_of(state.clone(), post_json(&state, "/api/clean", &body)).await;

        assert_eq!(result["dry_run"], true);
        assert_eq!(result["bytes_freed"], 0);
        assert!(blob.exists(), "a dry run must not delete");
    }

    #[tokio::test]
    async fn a_real_clean_removes_the_selected_candidate() {
        let (_tmp, state) = populated_state();
        let cache = state.paths.home_join(".npm/_cacache");

        let scan = json_of(state.clone(), get(state.clone(), "/api/scan?all=true")).await;
        let id = scan["candidates"][0]["id"].as_str().unwrap().to_string();

        let body = format!(r#"{{"ids":["{id}"],"dry_run":false}}"#);
        let result = json_of(state.clone(), post_json(&state, "/api/clean", &body)).await;

        assert_eq!(result["succeeded"], true);
        assert!(!cache.exists(), "the selected candidate should be gone");
    }

    #[tokio::test]
    async fn caution_items_are_skipped_unless_explicitly_confirmed() {
        // The browser must not be able to remove something irreplaceable with the
        // same request shape it uses for a cache.
        let (_tmp, state) = populated_state();
        let scan = json_of(state.clone(), get(state.clone(), "/api/scan?all=true")).await;

        // Force the stored candidate to caution tier, as a provider would.
        {
            let mut slot = state.last_scan.write().unwrap();
            let result = slot.as_mut().unwrap();
            for candidate in result.all.iter_mut() {
                candidate.tier = reclaim_core::Tier::Caution;
            }
        }

        let id = scan["candidates"][0]["id"].as_str().unwrap().to_string();
        let body = format!(r#"{{"ids":["{id}"],"dry_run":false}}"#);
        let result = json_of(state.clone(), post_json(&state, "/api/clean", &body)).await;

        assert_eq!(result["skipped_caution"], 1);
        assert!(
            state.paths.home_join(".npm/_cacache").exists(),
            "must not have deleted"
        );
    }

    #[tokio::test]
    async fn history_and_providers_endpoints_respond() {
        let (_tmp, state) = populated_state();

        let history = json_of(state.clone(), get(state.clone(), "/api/history")).await;
        assert!(history["runs"].is_array());

        let providers = json_of(state.clone(), get(state.clone(), "/api/providers")).await;
        let list = providers["providers"].as_array().unwrap();
        assert!(!list.is_empty());
        assert!(list
            .iter()
            .any(|p| p["id"].as_str().unwrap().starts_with("node.")));
    }

    #[tokio::test]
    async fn the_config_endpoint_reports_the_active_home_and_version() {
        let (_tmp, state) = populated_state();
        let config = json_of(state.clone(), get(state.clone(), "/api/config")).await;

        assert_eq!(config["home"], state.paths.home().display().to_string());
        assert_eq!(config["version"], reclaim_core::VERSION);
        assert!(config["config"]["scan"].is_object());
    }

    #[tokio::test]
    async fn candidate_views_carry_the_humanised_decision_signals() {
        // The browser must never have to re-derive these and drift from the CLI.
        let (_tmp, state) = populated_state();
        let scan = json_of(state.clone(), get(state.clone(), "/api/scan?all=true")).await;
        let candidate = &scan["candidates"][0];

        for key in [
            "tier",
            "regen",
            "last_used_human",
            "group_title",
            "warnings",
        ] {
            assert!(!candidate[key].is_null(), "missing `{key}` in {candidate}");
        }
        // Paths are contracted to `~/...` so the UI never shows an absolute home.
        let path = candidate["paths"][0].as_str().unwrap();
        assert!(path.starts_with("~/"), "path was not contracted: {path}");
    }

    #[tokio::test]
    async fn the_frontend_is_served_without_a_token() {
        // The page itself must load so it can then present the token from the URL.
        let (_tmp, state) = test_state();
        let (status, _) = send(
            state,
            HttpRequest::builder().uri("/").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
