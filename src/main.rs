mod agent;
mod audio;
mod chunker;
mod config;
mod llm;
mod metrics;
mod session;
mod stt;
mod task;
mod tls;
mod tts;

use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Html,
    routing::{get, post},
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tracing::{Level, error, info, warn};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

use crate::config::Config;

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    /// One permit per call in progress; a call holds its permit until it hangs up.
    calls: Arc<Semaphore>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_logging();
    tls::install();

    let config = Arc::new(Config::from_env()?);
    let app = App { calls: Arc::new(Semaphore::new(config.max_calls)), config: Arc::clone(&config) };

    let router = Router::new()
        .route("/", get(page))
        .route("/healthz", get(|| async { "ok" }))
        .route("/config", get(client_config))
        .route("/offer", post(offer))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    info!(
        "sol-li-ni listening on http://{} (max {} calls, {} s each{})",
        config.addr,
        config.max_calls,
        config.max_call_duration.as_secs(),
        if config.access_token.is_some() { ", access token required" } else { "" },
    );
    if config.access_token.is_none() && !config.addr.starts_with("127.") {
        warn!("no ACCESS_TOKEN set while listening beyond localhost: anyone who can reach this server can start calls on your API keys");
    }
    if config.allow_base_url_override {
        warn!("ALLOW_BASE_URL_OVERRIDE is on: callers can send OPENAI_API_KEY to any endpoint");
    }

    axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await?;
    info!("shut down");
    Ok(())
}

/// Our logs at `LOG_LEVEL` (default info); the WebRTC stack only when something goes wrong.
fn init_logging() {
    let level = std::env::var("LOG_LEVEL")
        .ok()
        .and_then(|l| l.parse::<Level>().ok())
        .unwrap_or(Level::INFO);

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("sol_li_ni", level)
                // Routine call noise: DTLS close-notify on hang-up, and ICE pinging before
                // the first candidate pair exists. Real failures still surface at ERROR.
                .with_target("rtc_dtls", Level::ERROR)
                .with_target("rtc_ice", Level::ERROR)
                .with_target("rtc::peer_connection::handler", Level::ERROR),
        )
        .init();
}

/// Ctrl+C, or SIGTERM from a process manager or container runtime.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutting down: no new calls");
}

/// The browser client. Dev builds read it from disk on every request, so editing the page needs
/// only a refresh — embedding it would force a recompile and relink for each tweak. Release
/// builds embed it, so the binary is self-contained.
async fn page() -> Result<Html<String>, (StatusCode, String)> {
    #[cfg(debug_assertions)]
    {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html");
        tokio::fs::read_to_string(path)
            .await
            .map(Html)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("reading {path}: {e}")))
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(Html(include_str!("../web/index.html").to_owned()))
    }
}

/// What the page needs to render its controls. Holds no secrets.
async fn client_config(State(app): State<App>) -> Json<Value> {
    let config = &app.config;
    Json(json!({
        "access_token_required": config.access_token.is_some(),
        "allow_base_url_override": config.allow_base_url_override,
        "max_call_secs": config.max_call_duration.as_secs(),
        "default_model": config.openai_model,
        "default_voice": config.murf_voice,
        "default_language": config.stt_language,
        "default_system_prompt": config.system_prompt,
    }))
}

/// Signaling: browser POSTs its SDP offer, we answer once ICE gathering is complete.
/// Per-call settings ride along as query parameters so the page can tune them per call.
async fn offer(
    State(app): State<App>,
    headers: HeaderMap,
    Query(settings): Query<session::CallSettings>,
    Json(offer): Json<RTCSessionDescription>,
) -> Result<Json<RTCSessionDescription>, (StatusCode, String)> {
    authorize(&app.config, &headers)?;
    settings
        .validate(app.config.allow_base_url_override)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let permit = Arc::clone(&app.calls).try_acquire_owned().map_err(|_| {
        warn!("refusing call: {} already in progress", app.config.max_calls);
        (StatusCode::SERVICE_UNAVAILABLE, "The agent is busy with other calls. Try again shortly.".to_owned())
    })?;

    session::accept_offer(app.config, settings, offer, permit).await.map(Json).map_err(|e| {
        error!("offer failed: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "Could not start the call.".to_owned())
    })
}

/// Checks `Authorization: Bearer <ACCESS_TOKEN>` when a token is configured.
fn authorize(config: &Config, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(expected) = &config.access_token else { return Ok(()) };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();

    if constant_time_eq(given.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "A valid access token is required.".to_owned()))
    }
}

/// Compares without an early exit, so response timing does not reveal how much of a guessed
/// token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_exactly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"secret"));
    }
}
