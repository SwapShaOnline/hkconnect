//! The remote (Streamable HTTP) server: OAuth routes plus `/mcp` behind a
//! bearer-token guard.

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::oauth::{OAuth, require_bearer};
use crate::tools::CarServer;

/// The whole HTTP application.
pub fn app(oauth: &Arc<OAuth>, server: CarServer, shutdown: CancellationToken) -> Router {
    // Only the public host (and loopback, for health checks on the box
    // itself) may be named in `Host`: a DNS-rebinding page can't reach us.
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts([
            oauth.config().host(),
            "localhost".into(),
            "127.0.0.1".into(),
            "::1".into(),
        ])
        .with_cancellation_token(shutdown);
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );
    let protected = Router::new()
        .nest_service("/mcp", mcp)
        .layer(from_fn_with_state(Arc::clone(oauth), require_bearer));
    oauth
        .routes()
        .merge(protected)
        .route("/healthz", get(|| async { "ok" }))
}
