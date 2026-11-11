//! `POST /api/clean`: remove the chosen candidates from the last scan.
//!
//! Two shapes of response. By default, one JSON object when the run is over.
//! With `?stream=true`, newline-delimited JSON: a `started` line, an
//! `item_started` and `item_finished` line per item as it happens, and a final
//! `finished` line carrying the same object the plain request returns. The
//! browser uses the stream to show progress; scripts can keep the simple form.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use reclaim_core::exec::{self, CleanEvent, CleanOptions};
use reclaim_core::journal::{ItemOutcome, RunRecord, Trigger};
use reclaim_core::model::Candidate;
use reclaim_core::progress::{sections, shown_bytes, OutcomeSection};
use reclaim_core::Paths;

use crate::api::ApiError;
use crate::state::ServerState;

/// Lines buffered between the clean thread and a slow client.
const STREAM_BUFFER: usize = 64;

#[derive(Debug, Deserialize)]
pub(crate) struct CleanRequest {
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

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CleanQuery {
    #[serde(default)]
    stream: bool,
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
    /// Items grouped by outcome, failures first, each largest first.
    sections: Vec<SectionView>,
}

#[derive(Debug, Serialize)]
struct CleanItemView {
    id: String,
    label: String,
    disposition: String,
    /// Which [`SectionView`] the item belongs to.
    section: &'static str,
    freed_bytes: u64,
    /// What the scan expected; shown for dry runs and failures.
    expected_bytes: u64,
    /// The size to display: what was freed, or what was expected when nothing was.
    shown_bytes: u64,
    /// Paths with the home directory shortened to `~`.
    paths: Vec<String>,
    /// The command that did the reclaiming, for items cleaned by a tool.
    command: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SectionView {
    key: &'static str,
    heading: &'static str,
    explanation: &'static str,
    bytes: u64,
    /// Item ids in display order.
    ids: Vec<String>,
}

/// One line of the `?stream=true` response.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamLine {
    Started { total: usize, bytes: u64 },
    ItemStarted { id: String, label: String },
    ItemFinished { item: CleanItemView },
    Finished { result: CleanResponse },
}

fn section_key(section: OutcomeSection) -> &'static str {
    match section {
        OutcomeSection::Failed => "failed",
        OutcomeSection::Removed => "removed",
        OutcomeSection::Command => "command",
        OutcomeSection::Trashed => "trashed",
        OutcomeSection::DryRun => "dry_run",
        OutcomeSection::AlreadyGone => "gone",
    }
}

fn item_view(item: &ItemOutcome, paths: &Paths) -> CleanItemView {
    CleanItemView {
        id: item.id.0.clone(),
        label: item.label.clone(),
        disposition: format!("{:?}", item.disposition).to_lowercase(),
        section: section_key(OutcomeSection::of(item.disposition)),
        freed_bytes: item.freed_bytes,
        expected_bytes: item.expected_bytes,
        shown_bytes: shown_bytes(item),
        paths: item.paths.iter().map(|p| paths.contract(p)).collect(),
        command: item.command.clone(),
        error: item.error.clone(),
    }
}

fn response(record: &RunRecord, skipped_caution: usize, paths: &Paths) -> CleanResponse {
    CleanResponse {
        dry_run: record.dry_run,
        bytes_freed: record.bytes_freed(),
        bytes_trashed: record.bytes_trashed(),
        succeeded: record.succeeded(),
        summary: record.summary(),
        skipped_caution,
        items: record.items.iter().map(|i| item_view(i, paths)).collect(),
        sections: sections(&record.items)
            .into_iter()
            .map(|(section, items)| SectionView {
                key: section_key(section),
                heading: section.heading(),
                explanation: section.explanation(),
                bytes: items.iter().map(|i| shown_bytes(i)).sum(),
                ids: items.iter().map(|i| i.id.0.clone()).collect(),
            })
            .collect(),
    }
}

/// A request that passed validation: what to remove and how.
struct Prepared {
    chosen: Vec<Candidate>,
    skipped_caution: usize,
    options: CleanOptions,
}

/// Resolve the ids against the last scan and drop unconfirmed caution items,
/// which may leave nothing to do.
fn prepare(state: &ServerState, request: &CleanRequest) -> Result<Prepared, ApiError> {
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

    Ok(Prepared {
        chosen,
        skipped_caution,
        options: CleanOptions {
            dry_run: request.dry_run,
            mode: state.config.delete.mode,
            trigger: Trigger::Web,
            concurrency: state.config.scan.threads(),
        },
    })
}

fn nothing_to_do(request: &CleanRequest, skipped_caution: usize) -> CleanResponse {
    CleanResponse {
        dry_run: request.dry_run,
        bytes_freed: 0,
        bytes_trashed: 0,
        succeeded: true,
        summary: "nothing to do: all selected items need explicit confirmation".into(),
        skipped_caution,
        items: Vec::new(),
        sections: Vec::new(),
    }
}

pub(crate) async fn clean(
    State(state): State<ServerState>,
    Query(query): Query<CleanQuery>,
    Json(request): Json<CleanRequest>,
) -> Result<Response, ApiError> {
    let prepared = prepare(&state, &request)?;
    if prepared.chosen.is_empty() {
        let result = nothing_to_do(&request, prepared.skipped_caution);
        return Ok(if query.stream {
            ndjson(single_line(StreamLine::Finished { result }))
        } else {
            Json(result).into_response()
        });
    }

    if query.stream {
        return Ok(stream(state, prepared));
    }

    let exec_state = state.clone();
    let Prepared {
        chosen,
        skipped_caution,
        options,
    } = prepared;
    let record = tokio::task::spawn_blocking(move || {
        exec::clean(&chosen, &exec_state.guard, &options, None)
    })
    .await
    .map_err(|e| ApiError::internal(format!("clean failed: {e}")))?;

    let _ = state.journal.write(&record);
    Ok(Json(response(&record, skipped_caution, &state.paths)).into_response())
}

/// Run the clean on a blocking thread, forwarding each event as a JSON line.
///
/// A client that disconnects mid-run does not stop it: items already started
/// finish, and the run is journalled either way.
fn stream(state: ServerState, prepared: Prepared) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(STREAM_BUFFER);

    tokio::task::spawn_blocking(move || {
        let Prepared {
            chosen,
            skipped_caution,
            options,
        } = prepared;
        let paths = state.paths.clone();
        let send = |line: StreamLine| {
            let _ = tx.blocking_send(to_line(&line));
        };

        let run = exec::spawn_clean(chosen, (*state.guard).clone(), options);
        for event in run.events.iter() {
            match event {
                CleanEvent::Started { total, bytes } => send(StreamLine::Started { total, bytes }),
                CleanEvent::ItemStarted { id, label } => {
                    send(StreamLine::ItemStarted { id: id.0, label })
                }
                CleanEvent::ItemFinished(item) => send(StreamLine::ItemFinished {
                    item: item_view(&item, &paths),
                }),
                // Sent below, once the run is journalled.
                CleanEvent::Finished(_) => {}
            }
        }
        let Ok(record) = run.join() else {
            tracing::error!("clean thread panicked");
            return;
        };
        let _ = state.journal.write(&record);
        send(StreamLine::Finished {
            result: response(&record, skipped_caution, &paths),
        });
    });

    let lines = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|line| (Ok::<_, Infallible>(line), rx))
    });
    ndjson(Body::from_stream(lines))
}

fn to_line(line: &StreamLine) -> String {
    let mut text = serde_json::to_string(line).unwrap_or_else(|e| {
        format!(r#"{{"type":"error","message":"could not encode progress: {e}"}}"#)
    });
    text.push('\n');
    text
}

fn single_line(line: StreamLine) -> Body {
    Body::from(to_line(&line))
}

fn ndjson(body: Body) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            // Stop proxies and the dev server from holding lines back.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
