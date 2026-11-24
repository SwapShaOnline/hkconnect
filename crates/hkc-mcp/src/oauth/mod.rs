//! A single-user OAuth 2.1 authorization server and resource-server guard
//! for the remote MCP endpoint.
//!
//! - Discovery: protected-resource metadata (RFC 9728) and
//!   authorization-server metadata (RFC 8414). Unauthenticated `/mcp`
//!   requests get `401` with `WWW-Authenticate: Bearer resource_metadata=…`.
//! - `/authorize`: authorization code flow with PKCE S256 (required), a
//!   one-field password page with a CSRF token bound to a cookie, attempt
//!   limiting, and no account enumeration (there is only one account).
//! - `/token`: `authorization_code` (single-use 60 s codes bound to client
//!   id, redirect URI and challenge; reuse revokes what the code issued) and
//!   `refresh_token` (rotation; reuse of a rotated token revokes the whole
//!   family).
//! - Clients: Client ID Metadata Documents ([`cimd`]), and, behind a flag,
//!   dynamic client registration. An optional allowlist restricts both.
//! - Tokens: opaque 256-bit values, stored only as SHA-256 hashes, bound to
//!   the `/mcp` resource; access 1 h, refresh 30 days.
//! - Abuse limits: sign-in attempts are charged before any password hashing
//!   and at most [`MAX_CONCURRENT_VERIFICATIONS`] hashes run at once; failed
//!   `/token` requests, client metadata fetches and open sign-in pages are
//!   capped per client address. `X-Forwarded-For` is believed only when the
//!   TCP peer is loopback or a configured trusted proxy.

pub mod cimd;
pub mod crypto;
pub mod limiter;
pub mod page;
pub mod state;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore};
use url::Url;

use self::cimd::{ClientInfo, Fetcher};
use self::crypto::{ct_eq, hash_token, is_s256_challenge, pkce_s256_matches, random_token};
use self::limiter::{Limiter, Window};
use self::state::{Kind, RegisteredClient, StateFile};
use crate::tools::AuthenticatedClient;

/// How long a sign-in page stays valid.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Most sign-in pages open at once.
const MAX_PENDING: usize = 100;
/// Most sign-in pages open at once from one client address; opening another
/// discards that address's oldest, never someone else's.
const MAX_PENDING_PER_ADDRESS: usize = 3;
/// Most password hashes computed at once (Argon2 uses ~19 MiB each); further
/// attempts are refused straight away rather than queued.
pub const MAX_CONCURRENT_VERIFICATIONS: usize = 2;
/// Client metadata documents fetched at once; further fetches are refused.
const MAX_CONCURRENT_FETCHES: usize = 4;
/// Window for the metadata fetch caps below.
const FETCH_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Uncached metadata fetches per client address per [`FETCH_WINDOW`].
const FETCHES_PER_ADDRESS: usize = 10;
/// Uncached metadata fetches overall per [`FETCH_WINDOW`].
const FETCHES_GLOBAL: usize = 60;
/// Most authorization codes outstanding (including used ones kept for reuse
/// detection).
const MAX_CODES: usize = 100;
/// Wrong passwords allowed on one sign-in page before it is discarded.
const MAX_ATTEMPTS_PER_PAGE: u32 = 5;
/// Most dynamically registered clients.
const MAX_REGISTERED: usize = 50;
/// Longest accepted password.
const MAX_PASSWORD_LEN: usize = 1024;

/// Settings for the OAuth server.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    issuer: String,
    /// Accept `/register` (dynamic client registration).
    pub allow_dcr: bool,
    /// When set, only these client ids may authorize.
    pub allowed_clients: Option<Vec<String>>,
    /// Take the client address from `X-Forwarded-For` (behind a proxy),
    /// when the TCP peer is loopback or in `trusted_proxies`.
    pub trust_proxy: bool,
    /// Besides loopback, the peer networks whose `X-Forwarded-For` is
    /// believed (with `trust_proxy`).
    pub trusted_proxies: Vec<Cidr>,
    /// Access token lifetime.
    pub access_ttl: SignedDuration,
    /// Refresh token lifetime.
    pub refresh_ttl: SignedDuration,
    /// Authorization code lifetime.
    pub code_ttl: Duration,
}

impl OAuthConfig {
    /// Settings for a server reachable at `public_url` (an origin such as
    /// `https://car.example.com`). Must be https, except on loopback for
    /// local testing.
    pub fn new(public_url: &str) -> Result<Self> {
        let url = Url::parse(public_url.trim()).context("--public-url is not a valid URL")?;
        let loopback = match url.host() {
            Some(url::Host::Domain(d)) => d == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            bail!("--public-url must be https:// (http is allowed only for localhost)");
        }
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            bail!(
                "--public-url must be just the origin, e.g. https://car.example.com \
                 (the MCP endpoint is always /mcp)"
            );
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("--public-url must not contain credentials");
        }
        Ok(Self {
            issuer: url.origin().ascii_serialization(),
            allow_dcr: false,
            allowed_clients: None,
            trust_proxy: false,
            trusted_proxies: Vec::new(),
            access_ttl: SignedDuration::from_hours(1),
            refresh_ttl: SignedDuration::from_hours(30 * 24),
            code_ttl: Duration::from_secs(60),
        })
    }

    /// The issuer: the public origin, without a trailing slash.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The protected resource: the MCP endpoint URL.
    pub fn resource(&self) -> String {
        format!("{}/mcp", self.issuer)
    }

    /// Where the protected-resource metadata is served.
    pub fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.issuer)
    }

    /// The public host (and port, if not the default), for `Host` checks.
    pub fn host(&self) -> String {
        Url::parse(&self.issuer)
            .ok()
            .and_then(|u| {
                let host = u.host_str()?.to_owned();
                Some(match u.port() {
                    Some(p) => format!("{host}:{p}"),
                    None => host,
                })
            })
            .unwrap_or_default()
    }

    /// Whether a TCP peer may set the client address with
    /// `X-Forwarded-For`: loopback, or a configured trusted proxy.
    pub fn is_trusted_proxy(&self, peer: IpAddr) -> bool {
        let peer = canonical(peer);
        peer.is_loopback() || self.trusted_proxies.iter().any(|c| c.contains(peer))
    }

    fn secure_cookies(&self) -> bool {
        self.issuer.starts_with("https://")
    }

    fn csrf_cookie(&self) -> &'static str {
        if self.secure_cookies() {
            "__Host-hkc_csrf"
        } else {
            "hkc_csrf"
        }
    }
}

/// An IPv4 or IPv6 network, such as `172.16.0.0/12` (a bare address is a
/// single host).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Whether `ip` is inside this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (addr, prefix) = s.split_once('/').unwrap_or((s, ""));
        let addr = canonical(
            addr.parse::<IpAddr>()
                .map_err(|_| format!("{s:?} is not an IP address or CIDR network"))?,
        );
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = if prefix.is_empty() {
            max
        } else {
            prefix
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("{s:?} has an invalid prefix length"))?
        };
        Ok(Self { addr, prefix })
    }
}

/// IPv4-mapped IPv6 addresses as plain IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// Parse a comma-separated list of networks.
pub fn parse_cidrs(list: &str) -> Result<Vec<Cidr>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect()
}

#[derive(Debug, Clone)]
struct Pending {
    client_id: String,
    /// The name the client gave itself (not verified).
    client_name: Option<String>,
    /// The host of a URL client id (verified by fetching it).
    client_host: Option<String>,
    /// The address (limiter key) that opened the page.
    opened_by: IpAddr,
    redirect_uri: String,
    state: Option<String>,
    challenge: String,
    resource: String,
    csrf: String,
    created: Instant,
    attempts: u32,
}

#[derive(Debug, Clone)]
struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    resource: String,
    expires: Instant,
    /// Set once exchanged: the token family it produced.
    used_by: Option<String>,
}

/// Counts password verifications (tests only).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct VerifyStats {
    total: std::sync::atomic::AtomicUsize,
    in_flight: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl VerifyStats {
    /// Count one verification for as long as the guard lives; it also
    /// lingers a little so that overlapping verifications really overlap.
    fn enter(self: &Arc<Self>) -> impl Drop {
        use std::sync::atomic::Ordering::SeqCst;
        struct Guard(Arc<VerifyStats>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.in_flight.fetch_sub(1, SeqCst);
            }
        }
        self.total.fetch_add(1, SeqCst);
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        Guard(Arc::clone(self))
    }

    /// (verifications so far, most at once).
    pub fn snapshot(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::SeqCst;
        (self.total.load(SeqCst), self.peak.load(SeqCst))
    }
}

/// The OAuth server.
#[derive(Debug)]
pub struct OAuth {
    config: OAuthConfig,
    state: StateFile,
    cimd: Fetcher,
    pending: Mutex<HashMap<String, Pending>>,
    codes: Mutex<HashMap<String, Code>>,
    limiter: std::sync::Mutex<Limiter>,
    token_limiter: std::sync::Mutex<Limiter>,
    verify_slots: Arc<Semaphore>,
    fetch_window: std::sync::Mutex<Window>,
    fetch_slots: Semaphore,
    #[cfg(test)]
    verify_stats: Arc<VerifyStats>,
}

fn locked<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl OAuth {
    /// A server using `state` for persistence.
    pub fn new(config: OAuthConfig, state: StateFile) -> Self {
        Self::with_fetcher(config, state, Fetcher::default())
    }

    /// As [`OAuth::new`], with a given metadata fetcher (tests).
    pub fn with_fetcher(config: OAuthConfig, state: StateFile, cimd: Fetcher) -> Self {
        Self {
            config,
            state,
            cimd,
            pending: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            limiter: std::sync::Mutex::new(Limiter::default()),
            token_limiter: std::sync::Mutex::new(Limiter::token()),
            verify_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_VERIFICATIONS)),
            fetch_window: std::sync::Mutex::new(Window::new(
                FETCH_WINDOW,
                FETCHES_PER_ADDRESS,
                FETCHES_GLOBAL,
            )),
            fetch_slots: Semaphore::new(MAX_CONCURRENT_FETCHES),
            #[cfg(test)]
            verify_stats: Arc::default(),
        }
    }

    /// The settings.
    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    /// Whether a password has been set.
    pub async fn has_password(&self) -> Result<bool> {
        self.state.read(|s| s.password_hash.is_some()).await
    }

    /// The OAuth routes (everything except `/mcp`).
    pub fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(authorization_server_metadata),
            )
            .route("/authorize", get(authorize_page).post(authorize_submit))
            .route("/token", post(token))
            .route("/register", post(register))
            .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
            .with_state(Arc::clone(self))
    }

    /// The client's metadata, or a message for the sign-in error page. The
    /// page never echoes what a remote server answered (that would make
    /// `/authorize` a blind probe); details go to the server log.
    async fn resolve_client(&self, client_id: &str, ip: IpAddr) -> Result<ClientInfo, String> {
        if client_id.is_empty() || client_id.len() > 2048 {
            return Err("missing or invalid client_id".into());
        }
        if let Some(allowed) = &self.config.allowed_clients
            && !allowed.iter().any(|a| a == client_id)
        {
            return Err(format!(
                "client {client_id:?} is not in this server's allowed client list \
                 (HKC_MCP_ALLOWED_CLIENTS)"
            ));
        }
        if cimd::is_url_client_id(client_id) {
            if let Some(info) = self.cimd.cached(client_id).await {
                return Ok(info);
            }
            if !locked(&self.fetch_window).take(ip, Instant::now()) {
                tracing::warn!(%ip, client_id, "client metadata fetch refused: rate limit");
                return Err("too many new clients; try again later".into());
            }
            let Ok(_slot) = self.fetch_slots.try_acquire() else {
                tracing::warn!(%ip, client_id, "client metadata fetch refused: busy");
                return Err("the server is busy; try again in a moment".into());
            };
            return self.cimd.resolve(client_id).await.map_err(|detail| {
                tracing::warn!(%ip, client_id, error = %detail, "client metadata rejected");
                "its client metadata document could not be fetched or is not valid \
                 (details are in the server log)"
                    .to_owned()
            });
        }
        if !self.config.allow_dcr {
            return Err(format!("unknown client {client_id:?}"));
        }
        let registered = self
            .state
            .read(|s| s.clients.get(client_id).cloned())
            .await
            .map_err(|e| format!("server state error: {e}"))?;
        registered
            .map(|c| ClientInfo {
                name: c.client_name,
                redirect_uris: c.redirect_uris,
            })
            .ok_or_else(|| format!("unknown client {client_id:?}"))
    }

    /// The client id an access token was issued to, if it is valid for this
    /// resource.
    async fn authenticate(&self, token: &str) -> Result<Option<String>> {
        let resource = self.config.resource();
        self.state
            .read(|s| {
                s.access(token, Timestamp::now())
                    .filter(|t| t.resource == resource)
                    .map(|t| t.client_id.clone())
            })
            .await
    }

    /// The client address for rate limiting: the TCP peer, or, with
    /// `trust_proxy` and a peer that is loopback or a trusted proxy, the
    /// address that proxy put in `X-Forwarded-For`.
    fn client_ip(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
        let Some(peer) = peer.map(|p| canonical(p.ip())) else {
            return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        };
        if self.config.trust_proxy
            && self.config.is_trusted_proxy(peer)
            && let Some(ip) = forwarded_for(headers)
        {
            return canonical(ip);
        }
        peer
    }
}

/// The address the nearest proxy saw: the last `X-Forwarded-For` entry.
/// Earlier entries are client-controlled and ignored.
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    let last = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|v| !v.is_empty())?;
    last.parse::<IpAddr>()
        .ok()
        .or_else(|| last.parse::<SocketAddr>().ok().map(|s| s.ip()))
}

/// The TCP peer address, when the server was started with connect info.
#[derive(Debug, Clone, Copy)]
pub struct Peer(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        std::future::ready(Ok(Self(peer)))
    }
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    json_response(
        status,
        &json!({ "error": error, "error_description": description }),
    )
}

fn server_error(e: &anyhow::Error) -> Response {
    tracing::error!(error = %e, "OAuth state error");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "internal error",
    )
}

/// Parameters as a map, rejecting repeated names (RFC 6749 §3.1).
fn single(pairs: Vec<(String, String)>) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for (k, v) in pairs {
        if map.insert(k.clone(), v).is_some() {
            return Err(format!("parameter {k:?} given more than once"));
        }
    }
    Ok(map)
}

fn same_resource(given: &str, ours: &str) -> bool {
    given.trim_end_matches('/') == ours
}

async fn protected_resource_metadata(State(o): State<Arc<OAuth>>) -> Response {
    json_response(
        StatusCode::OK,
        &json!({
            "resource": o.config.resource(),
            "authorization_servers": [o.config.issuer()],
            "bearer_methods_supported": ["header"],
            "resource_name": "hkc-mcp (Hyundai/Kia car)",
        }),
    )
}

async fn authorization_server_metadata(State(o): State<Arc<OAuth>>) -> Response {
    let issuer = o.config.issuer();
    let mut doc = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
    });
    if o.config.allow_dcr {
        doc["registration_endpoint"] = format!("{issuer}/register").into();
    }
    json_response(StatusCode::OK, &doc)
}

/// Redirect back to the client with `params` added to its redirect URI.
fn redirect_to(redirect_uri: &str, params: &[(&str, &str)]) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        return page::error(StatusCode::BAD_REQUEST, "The redirect URI is invalid.");
    };
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    page::secure_headers(headers);
    match HeaderValue::from_str(url.as_str()) {
        Ok(v) => {
            headers.insert(header::LOCATION, v);
            response
        }
        Err(_) => page::error(StatusCode::BAD_REQUEST, "The redirect URI is invalid."),
    }
}

fn redirect_error(
    o: &OAuth,
    redirect_uri: &str,
    state: Option<&str>,
    error: &str,
    description: &str,
) -> Response {
    let mut params = vec![
        ("error", error),
        ("error_description", description),
        ("iss", o.config.issuer()),
    ];
    if let Some(s) = state {
        params.push(("state", s));
    }
    redirect_to(redirect_uri, &params)
}

fn redirect_host(uri: &str) -> String {
    Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| uri.chars().take(100).collect())
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

fn set_csrf_cookie(o: &OAuth, response: &mut Response, value: Option<&str>) {
    let name = o.config.csrf_cookie();
    let secure = if o.config.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    let cookie = match value {
        Some(v) => format!("{name}={v}; Path=/; HttpOnly; SameSite=Strict; Max-Age=600{secure}"),
        None => format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}"),
    };
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, v);
    }
}

async fn authorize_page(
    State(o): State<Arc<OAuth>>,
    Peer(peer): Peer,
    headers: HeaderMap,
    axum::extract::Query(pairs): axum::extract::Query<Vec<(String, String)>>,
) -> Response {
    let ip = o.client_ip(&headers, peer);
    let params = match single(pairs) {
        Ok(p) => p,
        Err(e) => return page::error(StatusCode::BAD_REQUEST, &e),
    };
    let get = |k: &str| params.get(k).map(String::as_str);
    let client_id = get("client_id").unwrap_or("");
    let client = match o.resolve_client(client_id, ip).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(client_id, error = %e, "authorization refused: client");
            return page::error(StatusCode::BAD_REQUEST, &format!("Unknown client: {e}"));
        }
    };
    let Some(redirect_uri) = get("redirect_uri") else {
        return page::error(StatusCode::BAD_REQUEST, "redirect_uri is required.");
    };
    if !client.redirect_uris.iter().any(|u| u == redirect_uri) {
        tracing::warn!(
            client_id,
            "authorization refused: redirect_uri not registered"
        );
        return page::error(
            StatusCode::BAD_REQUEST,
            "The redirect_uri is not registered for this client.",
        );
    }
    // From here on errors go back to the (verified) client.
    let state = get("state");
    if get("response_type") != Some("code") {
        return redirect_error(
            &o,
            redirect_uri,
            state,
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    let challenge = get("code_challenge").unwrap_or("");
    if get("code_challenge_method") != Some("S256") || !is_s256_challenge(challenge) {
        return redirect_error(
            &o,
            redirect_uri,
            state,
            "invalid_request",
            "PKCE with code_challenge_method=S256 is required",
        );
    }
    let resource = o.config.resource();
    if let Some(r) = get("resource")
        && !same_resource(r, &resource)
    {
        return redirect_error(
            &o,
            redirect_uri,
            state,
            "invalid_target",
            "unknown resource",
        );
    }
    tracing::info!(client_id, "sign-in page shown");
    let request = random_token();
    let csrf = random_token();
    let pending = Pending {
        client_id: client_id.to_owned(),
        client_name: client.name.clone(),
        client_host: cimd::is_url_client_id(client_id)
            .then(|| redirect_host(client_id))
            .filter(|h| !h.is_empty()),
        opened_by: limiter::key(ip),
        redirect_uri: redirect_uri.to_owned(),
        state: state.map(str::to_owned),
        challenge: challenge.to_owned(),
        resource,
        csrf: csrf.clone(),
        created: Instant::now(),
        attempts: 0,
    };
    o.add_pending(request.clone(), pending.clone()).await;
    let mut response = sign_in_page(StatusCode::OK, &pending, &request, None);
    set_csrf_cookie(&o, &mut response, Some(&csrf));
    response
}

fn sign_in_page(
    status: StatusCode,
    pending: &Pending,
    request: &str,
    error: Option<&str>,
) -> Response {
    page::sign_in(
        status,
        &page::SignIn {
            client_id: &pending.client_id,
            client_host: pending.client_host.as_deref(),
            client_name: pending.client_name.as_deref(),
            redirect_host: &redirect_host(&pending.redirect_uri),
            request,
            csrf: &pending.csrf,
            error,
        },
    )
}

/// The sign-in form's fields.
#[derive(Debug, serde::Deserialize)]
struct SignInForm {
    request: String,
    csrf: String,
    password: String,
}

impl OAuth {
    fn limiter(&self) -> std::sync::MutexGuard<'_, Limiter> {
        locked(&self.limiter)
    }

    fn token_limiter(&self) -> std::sync::MutexGuard<'_, Limiter> {
        locked(&self.token_limiter)
    }

    /// Counts of password verifications so far (tests).
    #[cfg(test)]
    pub fn verify_stats(&self) -> (usize, usize) {
        self.verify_stats.snapshot()
    }

    /// Remember an open sign-in page.
    async fn add_pending(&self, request: String, pending: Pending) {
        let mut all = self.pending.lock().await;
        all.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        // One address can only displace its own pages, so it can't evict
        // the owner's sign-in in progress (short of using many addresses).
        let oldest = |all: &HashMap<String, Pending>, by: Option<IpAddr>| {
            all.iter()
                .filter(|(_, p)| by.is_none_or(|a| p.opened_by == a))
                .min_by_key(|(_, p)| p.created)
                .map(|(k, _)| k.clone())
        };
        let mine = all
            .values()
            .filter(|p| p.opened_by == pending.opened_by)
            .count();
        if mine >= MAX_PENDING_PER_ADDRESS
            && let Some(k) = oldest(&all, Some(pending.opened_by))
        {
            all.remove(&k);
        }
        if all.len() >= MAX_PENDING
            && let Some(k) = oldest(&all, None)
        {
            all.remove(&k);
        }
        all.insert(request, pending);
    }

    /// Charge one attempt to the sign-in page before hashing. `None` if the
    /// page is gone or has used up its attempts (it is then discarded).
    async fn reserve_page_attempt(&self, request: &str) -> Option<u32> {
        let mut all = self.pending.lock().await;
        let p = all.get_mut(request)?;
        if p.attempts >= MAX_ATTEMPTS_PER_PAGE {
            all.remove(request);
            return None;
        }
        p.attempts += 1;
        Some(p.attempts)
    }

    /// Give back a page attempt abandoned before hashing.
    async fn refund_page_attempt(&self, request: &str) {
        if let Some(p) = self.pending.lock().await.get_mut(request) {
            p.attempts = p.attempts.saturating_sub(1);
        }
    }

    /// Store a fresh single-use code for a completed sign-in.
    async fn issue_code(&self, pending: &Pending) -> String {
        let code = random_token();
        let mut codes = self.codes.lock().await;
        let now = Instant::now();
        // Used codes are kept one extra lifetime, to detect replays.
        codes.retain(|_, c| c.expires + self.config.code_ttl > now);
        if codes.len() >= MAX_CODES
            && let Some(oldest) = codes
                .iter()
                .min_by_key(|(_, c)| c.expires)
                .map(|(k, _)| k.clone())
        {
            codes.remove(&oldest);
        }
        codes.insert(
            hash_token(&code),
            Code {
                client_id: pending.client_id.clone(),
                redirect_uri: pending.redirect_uri.clone(),
                challenge: pending.challenge.clone(),
                resource: pending.resource.clone(),
                expires: now + self.config.code_ttl,
                used_by: None,
            },
        );
        code
    }

    /// A wrong password (already charged to the address and the page when
    /// reserved). The page is discarded after a few.
    async fn wrong_password(
        &self,
        ip: IpAddr,
        request: &str,
        pending: &Pending,
        attempt: u32,
    ) -> Response {
        tracing::warn!(%ip, client_id = %pending.client_id, "sign-in failed: wrong password");
        if attempt >= MAX_ATTEMPTS_PER_PAGE {
            self.pending.lock().await.remove(request);
            return page::error(
                StatusCode::UNAUTHORIZED,
                "Too many wrong passwords. Start again from the app that sent you here.",
            );
        }
        sign_in_page(
            StatusCode::UNAUTHORIZED,
            pending,
            request,
            Some("Wrong password."),
        )
    }
}

fn busy() -> Response {
    let mut r = page::error(
        StatusCode::TOO_MANY_REQUESTS,
        "The server is busy checking other sign-in attempts. Try again in a few seconds.",
    );
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    r
}

fn rate_limited(wait: Duration) -> Response {
    let secs = wait.as_secs().max(1);
    let mut r = page::error(
        StatusCode::TOO_MANY_REQUESTS,
        &format!("Too many sign-in attempts. Try again in {secs} seconds."),
    );
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    r
}

async fn authorize_submit(
    State(o): State<Arc<OAuth>>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Form(form): Form<SignInForm>,
) -> Response {
    let ip = o.client_ip(&headers, peer);
    // Checked before anything else, so a locked-out address costs nothing.
    if let Err(wait) = o.limiter().check(ip, Instant::now()) {
        tracing::warn!(%ip, "sign-in rate limited");
        return rate_limited(wait);
    }
    let pending = {
        let all = o.pending.lock().await;
        all.get(&form.request)
            .filter(|p| p.created.elapsed() < PENDING_TTL)
            .cloned()
    };
    let Some(pending) = pending else {
        return page::error(
            StatusCode::BAD_REQUEST,
            "This sign-in page has expired. Start again from the app that sent you here.",
        );
    };
    let cookie_ok =
        cookie(&headers, o.config.csrf_cookie()).is_some_and(|c| ct_eq(&c, &pending.csrf));
    if !cookie_ok || !ct_eq(&form.csrf, &pending.csrf) {
        tracing::warn!(%ip, "sign-in rejected: CSRF check failed");
        return page::error(
            StatusCode::FORBIDDEN,
            "The sign-in form could not be verified. Start again from the app that sent you here.",
        );
    }
    let hash = match o.state.read(|s| s.password_hash.clone()).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return page::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "No password is set. Run `hkc-mcp set-password` on the server.",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "could not read OAuth state");
            return page::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.");
        }
    };
    // Charge the attempt to the address (and globally) and to the page
    // *before* hashing: concurrent submissions can't all slip past the
    // limits while the first hashes are still running.
    let reservation = match o.limiter().reserve(ip, Instant::now()) {
        Ok(r) => r,
        Err(wait) => {
            tracing::warn!(%ip, "sign-in rate limited");
            return rate_limited(wait);
        }
    };
    let Some(attempt) = o.reserve_page_attempt(&form.request).await else {
        o.limiter().refund(reservation);
        return page::error(
            StatusCode::BAD_REQUEST,
            "This sign-in page has expired or has been used. Start again from the app that \
             sent you here.",
        );
    };
    // Bound the memory Argon2 can take: refuse rather than queue.
    let Ok(slot) = Arc::clone(&o.verify_slots).try_acquire_owned() else {
        o.limiter().refund(reservation);
        o.refund_page_attempt(&form.request).await;
        tracing::warn!(%ip, "sign-in refused: password checks busy");
        return busy();
    };
    let password = form.password;
    #[cfg(test)]
    let stats = Arc::clone(&o.verify_stats);
    let ok = password.len() <= MAX_PASSWORD_LEN
        && tokio::task::spawn_blocking(move || {
            // Held until the hash is done, even if the request is dropped.
            let _slot = slot;
            #[cfg(test)]
            let _counted = stats.enter();
            crypto::verify_password(&password, &hash)
        })
        .await
        .unwrap_or(false);
    if !ok {
        return o.wrong_password(ip, &form.request, &pending, attempt).await;
    }
    o.limiter().success(reservation);
    if o.pending.lock().await.remove(&form.request).is_none() {
        // Submitted twice at once; the other submission won.
        return page::error(
            StatusCode::BAD_REQUEST,
            "This sign-in page has already been used.",
        );
    }
    let code = o.issue_code(&pending).await;
    tracing::info!(%ip, client_id = %pending.client_id, "sign-in succeeded; code issued");
    let mut params = vec![("code", code.as_str()), ("iss", o.config.issuer())];
    if let Some(s) = &pending.state {
        params.push(("state", s));
    }
    let mut response = redirect_to(&pending.redirect_uri, &params);
    set_csrf_cookie(&o, &mut response, None);
    response
}

async fn token(
    State(o): State<Arc<OAuth>>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let ip = o.client_ip(&headers, peer);
    if let Err(wait) = o.token_limiter().check(ip, Instant::now()) {
        tracing::warn!(%ip, "token request rate limited");
        let secs = wait.as_secs().max(1);
        let mut r = oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "temporarily_unavailable",
            "too many failed token requests; try again later",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        return r;
    }
    let response = token_inner(&o, pairs).await;
    if response.status().is_client_error() {
        o.token_limiter().failure(ip, Instant::now());
    }
    response
}

async fn token_inner(o: &OAuth, pairs: Vec<(String, String)>) -> Response {
    let params = match single(pairs) {
        Ok(p) => p,
        Err(e) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", &e),
    };
    match params.get("grant_type").map(String::as_str) {
        Some("authorization_code") => exchange_code(o, &params).await,
        Some("refresh_token") => refresh(o, &params).await,
        Some(_) => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
        None => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "grant_type is required",
        ),
    }
}

fn token_response(o: &OAuth, issued: &state::IssuedTokens) -> Response {
    json_response(
        StatusCode::OK,
        &json!({
            "access_token": issued.access_token,
            "token_type": "Bearer",
            "expires_in": o.config.access_ttl.as_secs(),
            "refresh_token": issued.refresh_token,
        }),
    )
}

async fn exchange_code(o: &OAuth, params: &HashMap<String, String>) -> Response {
    let get = |k: &str| params.get(k).map_or("", String::as_str);
    let invalid = |d: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", d);
    let key = hash_token(get("code"));
    let mut codes = o.codes.lock().await;
    let Some(code) = codes.get(&key).cloned() else {
        return invalid("unknown or expired authorization code");
    };
    if let Some(family) = &code.used_by {
        // A code replayed: someone else may have it. Revoke what it issued.
        codes.remove(&key);
        drop(codes);
        tracing::warn!(client_id = %code.client_id, "authorization code reused; revoking its tokens");
        if let Err(e) = o.state.update(|s| s.revoke_family(family)).await {
            return server_error(&e);
        }
        return invalid("authorization code already used");
    }
    // Every other failure also consumes the code.
    let failure = if Instant::now() >= code.expires {
        Some("authorization code expired")
    } else if get("client_id") != code.client_id {
        Some("client_id does not match the authorization request")
    } else if get("redirect_uri") != code.redirect_uri {
        Some("redirect_uri does not match the authorization request")
    } else if !pkce_s256_matches(get("code_verifier"), &code.challenge) {
        Some("PKCE verification failed")
    } else {
        None
    };
    if let Some(reason) = failure {
        codes.remove(&key);
        tracing::warn!(client_id = %code.client_id, reason, "code exchange refused");
        return invalid(reason);
    }
    if let Some(r) = params.get("resource")
        && !same_resource(r, &code.resource)
    {
        codes.remove(&key);
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource does not match the authorization request",
        );
    }
    let family = random_token();
    if let Some(c) = codes.get_mut(&key) {
        c.used_by = Some(family.clone());
    }
    drop(codes);
    let config = &o.config;
    match o
        .state
        .update(|s| {
            s.issue(
                &family,
                &code.client_id,
                &code.resource,
                config.access_ttl,
                config.refresh_ttl,
            )
        })
        .await
    {
        Ok(issued) => {
            tracing::info!(client_id = %code.client_id, "tokens issued");
            token_response(o, &issued)
        }
        Err(e) => server_error(&e),
    }
}

enum RefreshOutcome {
    Issued(state::IssuedTokens),
    Refused(&'static str),
    ReuseDetected(String),
}

async fn refresh(o: &OAuth, params: &HashMap<String, String>) -> Response {
    let presented = params.get("refresh_token").cloned().unwrap_or_default();
    let client_id = params.get("client_id").cloned();
    let resource = params.get("resource").cloned();
    let config = &o.config;
    let result = o
        .state
        .update(|s| {
            let key = hash_token(&presented);
            let Some(record) = s.tokens.get(&key).cloned() else {
                return RefreshOutcome::Refused("unknown, expired or revoked refresh token");
            };
            if record.kind != Kind::Refresh || record.expires_at <= Timestamp::now() {
                return RefreshOutcome::Refused("unknown, expired or revoked refresh token");
            }
            if record.used {
                s.revoke_family(&record.family);
                return RefreshOutcome::ReuseDetected(record.client_id);
            }
            if client_id.as_ref().is_some_and(|c| *c != record.client_id) {
                return RefreshOutcome::Refused("refresh token was issued to another client");
            }
            if resource
                .as_ref()
                .is_some_and(|r| !same_resource(r, &record.resource))
            {
                return RefreshOutcome::Refused("resource does not match the refresh token");
            }
            if let Some(r) = s.tokens.get_mut(&key) {
                r.used = true;
            }
            RefreshOutcome::Issued(s.issue(
                &record.family,
                &record.client_id,
                &record.resource,
                config.access_ttl,
                config.refresh_ttl,
            ))
        })
        .await;
    match result {
        Ok(RefreshOutcome::Issued(issued)) => token_response(o, &issued),
        Ok(RefreshOutcome::Refused(reason)) => {
            let error = if reason.starts_with("resource") {
                "invalid_target"
            } else {
                "invalid_grant"
            };
            oauth_error(StatusCode::BAD_REQUEST, error, reason)
        }
        Ok(RefreshOutcome::ReuseDetected(client)) => {
            tracing::warn!(client_id = %client, "refresh token reused; revoked its whole family");
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "refresh token already used; all tokens from this sign-in were revoked",
            )
        }
        Err(e) => server_error(&e),
    }
}

/// Whether a client may use `uri` as a redirect URI: https, or http on
/// loopback, no fragment, at most 2048 bytes.
fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if url.fragment().is_some() || uri.len() > 2048 {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => {
            matches!(url.host(), Some(url::Host::Domain("localhost")))
                || matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
                || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        }
        _ => false,
    }
}

async fn register(State(o): State<Arc<OAuth>>, body: axum::body::Bytes) -> Response {
    let bad = |d: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", d);
    if !o.config.allow_dcr {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "dynamic client registration is disabled on this server",
        );
    }
    let Ok(meta) = serde_json::from_slice::<Value>(&body) else {
        return bad("body must be a JSON object");
    };
    let redirect_uris: Vec<String> = match meta.get("redirect_uris").and_then(Value::as_array) {
        Some(list) if !list.is_empty() && list.len() <= 10 => {
            match list.iter().map(|v| v.as_str().map(str::to_owned)).collect() {
                Some(uris) => uris,
                None => return bad("redirect_uris must be strings"),
            }
        }
        _ => return bad("redirect_uris must list 1 to 10 URIs"),
    };
    if !redirect_uris.iter().all(|u| valid_redirect_uri(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect URIs must be https, or http on localhost, without a fragment",
        );
    }
    match meta
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str)
    {
        None | Some("none") => {}
        Some(_) => {
            return bad("only public clients (token_endpoint_auth_method=none) are supported");
        }
    }
    let allowed = |key: &str, ok: &[&str]| {
        meta.get(key).and_then(Value::as_array).is_none_or(|list| {
            list.iter()
                .all(|v| v.as_str().is_some_and(|s| ok.contains(&s)))
        })
    };
    if !allowed("grant_types", &["authorization_code", "refresh_token"])
        || !allowed("response_types", &["code"])
    {
        return bad("only the authorization_code and refresh_token grants are supported");
    }
    let client_name = meta
        .get("client_name")
        .and_then(Value::as_str)
        .map(|n| n.chars().take(100).collect::<String>());
    let client_id = format!("hkc-{}", random_token());
    let record = RegisteredClient {
        redirect_uris: redirect_uris.clone(),
        client_name: client_name.clone(),
        registered_at: Timestamp::now(),
    };
    let id = client_id.clone();
    let saved = o
        .state
        .update(move |s| {
            if s.clients.len() >= MAX_REGISTERED {
                return false;
            }
            s.clients.insert(id, record);
            true
        })
        .await;
    match saved {
        Ok(true) => {
            tracing::info!(%client_id, "client registered");
            json_response(
                StatusCode::CREATED,
                &json!({
                    "client_id": client_id,
                    "client_id_issued_at": Timestamp::now().as_second(),
                    "client_name": client_name,
                    "redirect_uris": redirect_uris,
                    "token_endpoint_auth_method": "none",
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                }),
            )
        }
        Ok(false) => bad("too many registered clients; run `hkc-mcp revoke-all --clients`"),
        Err(e) => server_error(&e),
    }
}

/// Middleware for `/mcp`: require a valid bearer token for this resource,
/// and record which client it belongs to.
pub async fn require_bearer(
    State(o): State<Arc<OAuth>>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let Some(token) = token else {
        return challenge(&o, None);
    };
    match o.authenticate(&token).await {
        Ok(Some(client_id)) => {
            request
                .extensions_mut()
                .insert(AuthenticatedClient(client_id));
            next.run(request).await
        }
        Ok(None) => challenge(&o, Some("invalid_token")),
        Err(e) => server_error(&e),
    }
}

fn challenge(o: &OAuth, error: Option<&str>) -> Response {
    let mut value = format!(
        "Bearer resource_metadata=\"{}\"",
        o.config.resource_metadata_url()
    );
    if let Some(e) = error {
        value.push_str(", error=\"");
        value.push_str(e);
        value.push_str("\", error_description=\"the access token is invalid or expired\"");
    }
    let mut response = json_response(
        StatusCode::UNAUTHORIZED,
        &json!({ "error": error.unwrap_or("unauthorized") }),
    );
    if let Ok(v) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    response
}

/// `hkc-mcp revoke-all`: drop every issued token (and, if asked, every
/// registered client). Returns how many tokens were removed.
pub async fn revoke_all(state: &StateFile, clients: bool) -> Result<(usize, usize)> {
    state
        .update(|s| {
            let tokens = s.tokens.len();
            s.tokens.clear();
            let registered = if clients {
                let n = s.clients.len();
                s.clients.clear();
                n
            } else {
                0
            };
            (tokens, registered)
        })
        .await
}

/// Store a new password hash; optionally revoke all tokens.
pub async fn set_password(state: &StateFile, password: &str, revoke: bool) -> Result<usize> {
    let owned = password.to_owned();
    let hash = tokio::task::spawn_blocking(move || crypto::hash_password(&owned)).await??;
    state
        .update(|s| {
            s.password_hash = Some(hash);
            if revoke {
                let n = s.tokens.len();
                s.tokens.clear();
                n
            } else {
                0
            }
        })
        .await
}

/// What [`sync_password`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSync {
    /// No password was stored; this one now is.
    Set,
    /// The stored password already was this one; nothing changed.
    Unchanged,
    /// A different password was stored: it was replaced and this many
    /// issued tokens were revoked.
    Rotated { revoked: usize },
}

/// Make `password` the sign-in password, for a password supplied by the
/// environment (`HKC_MCP_PASSWORD`) on every start: store it when none is
/// stored, leave the state alone when it already matches, and otherwise
/// replace it and revoke every token, as `set-password` does by default.
pub async fn sync_password(state: &StateFile, password: &str) -> Result<PasswordSync> {
    let Some(hash) = state.read(|s| s.password_hash.clone()).await? else {
        set_password(state, password, false).await?;
        return Ok(PasswordSync::Set);
    };
    let owned = password.to_owned();
    let same = tokio::task::spawn_blocking(move || crypto::verify_password(&owned, &hash)).await?;
    if same {
        return Ok(PasswordSync::Unchanged);
    }
    let revoked = set_password(state, password, true).await?;
    Ok(PasswordSync::Rotated { revoked })
}

#[cfg(test)]
mod tests;
