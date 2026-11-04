mod api_scan;
mod api_search;
mod api_stats;
mod auth;
mod ch_client;
mod config;

use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::config::Config;

/// Shared application state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub ch: Arc<ch_client::ClickHouse>,
    /// Write-capable ClickHouse client (dust_writer). Used for INSERTs the API
    /// performs itself (precomputed dashboard stats into scan_stats). Read
    /// queries use `ch` (readonly); only writes use `ch_write`.
    pub ch_write: Arc<ch_client::ClickHouse>,
    pub auth: Arc<auth::AuthService>,
    /// In-flight scans keyed by scan_id: { scan_id -> RunningScan }.
    /// Used to cancel scans and to detect duplicate scan_ids.
    pub running_scans: Arc<std::sync::Mutex<std::collections::HashMap<String, crate::api_scan::RunningScan>>>,
    /// Finished scans keyed by scan_id: { scan_id -> FinishedScan }.
    /// Records the real outcome (done/failed/cancelled) from the child exit
    /// code, so the status endpoint doesn't have to guess from row timestamps.
    pub finished_scans: Arc<std::sync::Mutex<std::collections::HashMap<String, crate::api_scan::FinishedScan>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load()?;
    let config = Arc::new(config);

    let ch = Arc::new(ch_client::ClickHouse::new(
        config.ch_url.clone(),
        config.ch_read_user.clone(),
        config.ch_read_password.clone(),
        config.ch_table.clone(),
    ));

    let ch_write = Arc::new(ch_client::ClickHouse::new(
        config.ch_url.clone(),
        config.ch_write_user.clone(),
        config.ch_write_password.clone(),
        config.ch_table.clone(),
    ));

    let auth = Arc::new(auth::AuthService::new(
        config.clone(),
        ch.clone(),
    )?);

    let state = AppState {
        config: config.clone(),
        ch,
        ch_write,
        auth,
        running_scans: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        finished_scans: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };

    // Ensure scan log dir exists.
    std::fs::create_dir_all(&config.scan_log_dir).ok();

    // Public routes (no auth): login.
    let public_routes = Router::new().merge(auth::routes());

    // Protected routes: require a valid Bearer token.
    let protected_routes = Router::new()
        .merge(api_scan::routes())
        .merge(api_search::routes())
        .merge(api_stats::routes())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let api_routes = Router::new().merge(public_routes).merge(protected_routes);

    let app = Router::new()
        .nest("/api", api_routes)
        .fallback_service(
            ServeDir::new(&config.web_dir)
                .fallback(tower_http::services::ServeFile::new(
                    std::path::Path::new(&config.web_dir).join("index.html"),
                )),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    eprintln!("dust-serve listening on http://{}", config.listen);
    eprintln!("serving frontend from {}", config.web_dir);
    axum::serve(listener, app).await?;
    Ok(())
}
