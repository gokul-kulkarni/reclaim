//! Serving the embedded frontend.
//!
//! `web/dist` is baked into the binary so `reclaim ui` needs no separate files.
//! When the frontend has not been built, a self-contained fallback page is served
//! instead — the binary must always run, even from a fresh `cargo install` that
//! never had Node available.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::state::ServerState;

#[derive(rust_embed::Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
struct Frontend;

pub async fn serve(State(state): State<ServerState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Frontend::get(path) {
        return file_response(path, file.data.into_owned());
    }

    // Single-page app: unknown paths fall through to index.html so client-side
    // routing works on a refresh.
    if let Some(index) = Frontend::get("index.html") {
        return file_response("index.html", index.data.into_owned());
    }

    if state.dev {
        return (
            StatusCode::NOT_FOUND,
            "Dev mode: run `npm --prefix web run dev` and use the Vite URL instead.",
        )
            .into_response();
    }

    fallback_page()
}

fn file_response(path: &str, body: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        // The assets are versioned with the binary, so no caching across runs.
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Served when the binary was built without the frontend assets.
fn fallback_page() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(FALLBACK_HTML))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

const FALLBACK_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>reclaim</title>
<style>
  :root { color-scheme: light dark; }
  body {
    font: 15px/1.6 ui-sans-serif, -apple-system, "Segoe UI", sans-serif;
    max-width: 42rem; margin: 4rem auto; padding: 0 1.5rem;
  }
  code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .9em; }
  pre { background: rgba(128,128,128,.12); padding: 1rem; border-radius: .5rem; overflow-x: auto; }
  h1 { font-size: 1.4rem; }
</style>
</head>
<body>
  <h1>reclaim — frontend not built</h1>
  <p>
    The server is running, but this binary was compiled without the web UI assets.
    Build them once and rebuild:
  </p>
  <pre>npm --prefix web ci
npm --prefix web run build
cargo build --release</pre>
  <p>Everything works without the web UI in the meantime:</p>
  <pre>reclaim            # interactive terminal UI
reclaim scan       # plain report
reclaim clean --tier safe --yes</pre>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_page_is_self_contained() {
        // It must render with no network and no external assets, since it is what
        // a user sees when the build chain was incomplete.
        assert!(FALLBACK_HTML.starts_with("<!doctype html>"));
        assert!(!FALLBACK_HTML.contains("http://"));
        assert!(!FALLBACK_HTML.contains("https://"));
        assert!(FALLBACK_HTML.contains("npm --prefix web run build"));
    }

    #[test]
    fn the_fallback_page_still_tells_the_user_what_does_work() {
        assert!(FALLBACK_HTML.contains("reclaim scan"));
    }

    #[test]
    fn mime_types_are_derived_from_the_path() {
        let response = file_response("app.js", b"// x".to_vec());
        let mime = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(mime.to_str().unwrap().contains("javascript"), "{mime:?}");

        let response = file_response("index.html", b"<html>".to_vec());
        let mime = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(mime.to_str().unwrap().contains("html"));
    }
}
