//! The local web UI.
//!
//! Security posture, since this server can delete files:
//!
//! - It binds `127.0.0.1` only, and refuses to bind anything else.
//! - Every API route requires a token minted at startup and printed in the URL.
//!   Without it, a page in the user's browser could otherwise reach the API
//!   through DNS rebinding or a stray fetch from another local process.
//! - The `Origin` header must be a loopback origin, which blocks cross-site
//!   requests from a page the user happens to have open.
//! - The process exits with the CLI invocation; there is no lingering daemon.

mod api;
mod assets;
mod state;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use reclaim_core::config::Config;
use reclaim_core::Paths;

pub use state::{ServerState, Token};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// 0 picks a free port.
    pub port: u16,
    pub open_browser: bool,
    /// Serve from a Vite dev server rather than the embedded assets.
    pub dev: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            port: 0,
            open_browser: true,
            dev: false,
        }
    }
}

/// Start the server and block until Ctrl-C.
pub fn serve(paths: Paths, config: Config, options: ServeOptions) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(serve_async(paths, config, options))
}

async fn serve_async(paths: Paths, config: Config, options: ServeOptions) -> Result<()> {
    let state = ServerState::new(paths, config, options.dev);
    let token = state.token.clone();

    let router = api::router(state);

    // Loopback only. This is not configurable: a server that can delete files
    // has no business listening on a routable address.
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), options.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let bound = listener.local_addr().context("reading the bound address")?;

    let url = format!("http://127.0.0.1:{}/?t={}", bound.port(), token.as_str());
    println!("reclaim web UI: {url}");
    println!("This link is single-use for this process. Press Ctrl-C to stop.");

    if options.open_browser {
        open_browser(&url);
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nShutting down.");
        })
        .await
        .context("serving")?;

    Ok(())
}

/// Best-effort browser launch. Never fatal: the URL is already printed.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return;
    };

    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_options_pick_a_free_port_and_open_a_browser() {
        let options = ServeOptions::default();
        assert_eq!(options.port, 0);
        assert!(options.open_browser);
        assert!(!options.dev);
    }
}
