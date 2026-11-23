//! End-to-end OAuth tests, in process: the real router driven with
//! `tower::ServiceExt::oneshot`, a local fake CIMD server, and a fake
//! Bluelink backend for the MCP calls.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::cimd::Fetcher;
use super::state::StateFile;
use super::{OAuth, OAuthConfig, crypto};
use crate::policy::Policy;
use crate::tests as tool_tests;

const PUBLIC: &str = "https://car.example.com";
const HOST: &str = "car.example.com";
const PASSWORD: &str = "correct horse battery staple";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

struct Harness {
    app: Router,
    oauth: Arc<OAuth>,
    client_id: String,
    dir: tempfile::TempDir,
    _cimd: MockServer,
    backend: MockServer,
}

async fn harness() -> Harness {
    harness_with(|_| {}).await
}

async fn harness_with(configure: impl FnOnce(&mut OAuthConfig)) -> Harness {
    let cimd = MockServer::start().await;
    let client_id = format!("{}/oauth/client.json", cimd.uri());
    Mock::given(method("GET"))
        .and(path("/oauth/client.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": client_id,
            "client_name": "Claude <test>",
            "redirect_uris": [REDIRECT],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
        })))
        .mount(&cimd)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let state = StateFile::new(dir.path().join("mcp-oauth.json"));
    super::set_password(&state, PASSWORD, false).await.unwrap();
    let mut config = OAuthConfig::new(PUBLIC).unwrap();
    configure(&mut config);
    let oauth = Arc::new(OAuth::with_fetcher(
        config,
        state,
        Fetcher::allowing_local(),
    ));
    let backend = tool_tests::backend().await;
    let server = tool_tests::car_server(
        &backend,
        &Policy::default(),
        Some(&dir.path().join("mcp-audit.jsonl")),
    );
    let app = crate::http::app(&oauth, server, CancellationToken::new());
    Harness {
        app,
        oauth,
        client_id,
        dir,
        _cimd: cimd,
        backend,
    }
}

fn pkce() -> (String, String) {
    let verifier = crypto::random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn get(&self, uri: &str) -> Response {
        self.send(
            Request::get(uri)
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    fn authorize_query(&self, challenge: &str, extra: &[(&str, &str)]) -> String {
        let mut pairs = vec![
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "xyz"),
            ("resource", "https://car.example.com/mcp"),
        ];
        for (k, v) in extra {
            pairs.retain(|(pk, _)| pk != k);
            if !v.is_empty() {
                pairs.push((k, v));
            }
        }
        format!("/authorize?{}", form(&pairs))
    }

    /// Open the sign-in page; returns (request id, csrf, cookie header).
    async fn open_page(&self, challenge: &str) -> (String, String, String) {
        let response = self.get(&self.authorize_query(challenge, &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::X_FRAME_OPTIONS],
            "DENY",
            "sign-in page must not be framable"
        );
        let set_cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.starts_with("__Host-hkc_csrf="), "{set_cookie}");
        assert!(set_cookie.contains("Secure") && set_cookie.contains("HttpOnly"));
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let html = text(response).await;
        // The client name is shown, escaped.
        if self.client_id.starts_with("http") {
            assert!(html.contains("Claude &lt;test&gt;"), "{html}");
        }
        assert!(!html.contains("<test>"), "{html}");
        let field = |name: &str| {
            let marker = format!("name=\"{name}\" value=\"");
            let start = html.find(&marker).unwrap() + marker.len();
            html[start..].split('"').next().unwrap().to_owned()
        };
        (field("request"), field("csrf"), cookie)
    }

    async fn submit(
        &self,
        request_id: &str,
        csrf: &str,
        cookie: Option<&str>,
        password: &str,
        forwarded_for: Option<&str>,
    ) -> Response {
        let mut builder = Request::post("/authorize")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        if let Some(xff) = forwarded_for {
            builder = builder.header("x-forwarded-for", xff);
        }
        let mut request = builder
            .body(Body::from(form(&[
                ("request", request_id),
                ("csrf", csrf),
                ("password", password),
            ])))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
        self.send(request).await
    }

    /// Sign in and return the authorization code.
    async fn code(&self, challenge: &str) -> String {
        let (request_id, csrf, cookie) = self.open_page(challenge).await;
        let response = self
            .submit(&request_id, &csrf, Some(&cookie), PASSWORD, None)
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = location(&response);
        assert!(location.starts_with(REDIRECT), "{location}");
        let url = url::Url::parse(&location).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["state"], "xyz");
        assert_eq!(q["iss"], PUBLIC);
        q["code"].clone()
    }

    async fn token(&self, pairs: &[(&str, &str)]) -> (StatusCode, Value) {
        let response = self
            .send(
                Request::post("/token")
                    .header(header::HOST, HOST)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form(pairs)))
                    .unwrap(),
            )
            .await;
        let status = response.status();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        (status, serde_json::from_str(&text(response).await).unwrap())
    }

    async fn exchange(&self, code: &str, verifier: &str) -> (StatusCode, Value) {
        let client_id = self.client_id.clone();
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", &client_id),
            ("code_verifier", verifier),
            ("resource", "https://car.example.com/mcp"),
        ])
        .await
    }

    async fn refresh(&self, refresh_token: &str) -> (StatusCode, Value) {
        let client_id = self.client_id.clone();
        self.token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &client_id),
        ])
        .await
    }

    /// Full sign-in: returns the token response.
    async fn sign_in(&self) -> Value {
        let (verifier, challenge) = pkce();
        let code = self.code(&challenge).await;
        let (status, body) = self.exchange(&code, &verifier).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn mcp(&self, token: Option<&str>, body: Value, session: Option<&str>) -> Response {
        let mut builder = Request::post("/mcp")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(s) = session {
            builder = builder
                .header("mcp-session-id", s)
                .header("mcp-protocol-version", "2025-06-18");
        }
        self.send(builder.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn mcp_status(&self, token: &str) -> StatusCode {
        self.mcp(Some(token), initialize(), None).await.status()
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" }
        }
    })
}

async fn text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn location(response: &Response) -> String {
    response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned()
}

/// The JSON-RPC messages in an SSE (or plain JSON) body.
fn messages(body: &str) -> Vec<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return vec![v];
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

// ----- discovery -----

#[tokio::test]
async fn metadata_documents() {
    let h = harness().await;
    for uri in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let response = h.get(uri).await;
        assert_eq!(response.status(), StatusCode::OK);
        let doc: Value = serde_json::from_str(&text(response).await).unwrap();
        assert_eq!(doc["resource"], "https://car.example.com/mcp");
        assert_eq!(doc["authorization_servers"], json!([PUBLIC]));
    }
    let doc: Value =
        serde_json::from_str(&text(h.get("/.well-known/oauth-authorization-server").await).await)
            .unwrap();
    assert_eq!(doc["issuer"], PUBLIC);
    assert_eq!(
        doc["authorization_endpoint"],
        "https://car.example.com/authorize"
    );
    assert_eq!(doc["token_endpoint"], "https://car.example.com/token");
    assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(doc["client_id_metadata_document_supported"], true);
    assert!(doc.get("registration_endpoint").is_none());
}

#[tokio::test]
async fn unauthenticated_mcp_gets_a_discovery_challenge() {
    let h = harness().await;
    let response = h.mcp(None, initialize(), None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        challenge,
        "Bearer resource_metadata=\"https://car.example.com/.well-known/oauth-protected-resource/mcp\""
    );

    let response = h.mcp(Some("made-up"), initialize(), None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap();
    assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");
    assert!(challenge.contains("resource_metadata="), "{challenge}");
}

// ----- the happy path, then everything that must fail -----

#[tokio::test]
async fn full_flow_reaches_mcp_and_audits_the_client() {
    let h = harness().await;
    let tokens = h.sign_in().await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(access.len(), 43);

    // Tokens are stored only as hashes.
    let state = std::fs::read_to_string(h.dir.path().join("mcp-oauth.json")).unwrap();
    assert!(!state.contains(access));
    assert!(!state.contains(tokens["refresh_token"].as_str().unwrap()));
    assert!(!state.contains(PASSWORD));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(h.dir.path().join("mcp-oauth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // MCP over HTTP: initialize, initialized, then a command.
    let response = h.mcp(Some(access), initialize(), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let session = response.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let init = messages(&text(response).await);
    assert_eq!(init[0]["result"]["serverInfo"]["name"], "hkc-mcp");
    let response = h
        .mcp(
            Some(access),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            Some(&session),
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());

    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_tests::fixture("vehicles")))
        .mount(&h.backend)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{}/control/door",
            tool_tests::VEHICLE
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "retCode": "S", "resCode": "0000", "resMsg": null, "msgId": "cmd-http"
        })))
        .expect(1)
        .mount(&h.backend)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v1/spa/notifications/{}/records",
            tool_tests::VEHICLE
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "retCode": "S", "resCode": "0000", "msgId": "x",
            "resMsg": [{ "recordId": "cmd-http", "result": "success" }]
        })))
        .mount(&h.backend)
        .await;
    let response = h
        .mcp(
            Some(access),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "lock", "arguments": {} }
            }),
            Some(&session),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replies = messages(&text(response).await);
    let reply = replies.iter().find(|m| m["id"] == 2).expect("tool reply");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let audit = std::fs::read_to_string(h.dir.path().join("mcp-audit.jsonl")).unwrap();
    let line: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
    assert_eq!(line["client"], h.client_id.as_str());
    assert_eq!(line["outcome"], "succeeded");
    assert!(!audit.contains(access));

    // A valid token with a session from another token still needs to be
    // valid itself: a bogus token is refused even with the session id.
    let response = h
        .mcp(
            Some("bogus"),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
            Some(&session),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_password_is_refused_and_the_page_stays_usable() {
    let h = harness().await;
    let (_, challenge) = pkce();
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let response = h
        .submit(&request_id, &csrf, Some(&cookie), "wrong password!!", None)
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(header::LOCATION).is_none());
    assert!(text(response).await.contains("Wrong password."));
    let response = h
        .submit(&request_id, &csrf, Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    // The page is single use.
    let response = h
        .submit(&request_id, &csrf, Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn csrf_token_and_cookie_are_both_required() {
    let h = harness().await;
    let (_, challenge) = pkce();
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let no_cookie = h.submit(&request_id, &csrf, None, PASSWORD, None).await;
    assert_eq!(no_cookie.status(), StatusCode::FORBIDDEN);
    let wrong_field = h
        .submit(&request_id, "forged", Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(wrong_field.status(), StatusCode::FORBIDDEN);
    let wrong_cookie = h
        .submit(
            &request_id,
            &csrf,
            Some("__Host-hkc_csrf=forged"),
            PASSWORD,
            None,
        )
        .await;
    assert_eq!(wrong_cookie.status(), StatusCode::FORBIDDEN);
    let unknown_request = h
        .submit("no-such-request", &csrf, Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(unknown_request.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pkce_is_required_and_checked() {
    let h = harness().await;
    let (verifier, challenge) = pkce();
    for extra in [
        vec![("code_challenge", "")],
        vec![("code_challenge_method", "plain")],
        vec![("code_challenge_method", "")],
    ] {
        let response = h.get(&h.authorize_query(&challenge, &extra)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{extra:?}");
        let loc = location(&response);
        assert!(loc.starts_with(REDIRECT), "{loc}");
        assert!(loc.contains("error=invalid_request"), "{loc}");
        assert!(loc.contains("state=xyz"), "{loc}");
    }

    let code = h.code(&challenge).await;
    let (status, body) = h.exchange(&code, &pkce().0).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    // A failed attempt burns the code.
    let (status, _) = h.exchange(&code, &verifier).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn code_reuse_revokes_what_it_issued() {
    let h = harness().await;
    let (verifier, challenge) = pkce();
    let code = h.code(&challenge).await;
    let (status, tokens) = h.exchange(&code, &verifier).await;
    assert_eq!(status, StatusCode::OK);
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(h.mcp_status(access).await, StatusCode::OK);
    let (status, body) = h.exchange(&code, &verifier).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(h.mcp_status(access).await, StatusCode::UNAUTHORIZED);
    let (status, _) = h.refresh(tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn redirect_uri_and_client_must_match() {
    let h = harness().await;
    let (verifier, challenge) = pkce();
    // Unregistered redirect: an error page, never a redirect.
    let response = h
        .get(&h.authorize_query(&challenge, &[("redirect_uri", "https://evil.example/cb")]))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(header::LOCATION).is_none());
    // Unknown client: also an error page.
    let response = h
        .get(&h.authorize_query(&challenge, &[("client_id", "some-client")]))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // Repeated parameters are refused.
    let q = h.authorize_query(&challenge, &[]);
    let response = h.get(&format!("{q}&state=again")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let code = h.code(&challenge).await;
    let client_id = h.client_id.clone();
    let (status, body) = h
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "https://claude.ai/other"),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
        ])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    let code = h.code(&challenge).await;
    let (status, body) = h
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("client_id", "https://other.example/client.json"),
            ("code_verifier", &verifier),
        ])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn wrong_resource_is_refused() {
    let h = harness().await;
    let (verifier, challenge) = pkce();
    let response = h
        .get(&h.authorize_query(&challenge, &[("resource", "https://evil.example/mcp")]))
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(location(&response).contains("error=invalid_target"));

    let code = h.code(&challenge).await;
    let client_id = h.client_id.clone();
    let (status, body) = h
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", "https://evil.example/mcp"),
        ])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_target");

    let tokens = h.sign_in().await;
    let (status, body) = h
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("resource", "https://evil.example/mcp"),
        ])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_target");

    // A token minted for another resource (e.g. by an older config) is
    // refused at /mcp even though it exists.
    let other = h
        .oauth
        .state
        .update(|s| {
            s.issue(
                "fam-x",
                "c",
                "https://elsewhere.example/mcp",
                jiff::SignedDuration::from_hours(1),
                jiff::SignedDuration::from_hours(1),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        h.mcp_status(&other.access_token).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn codes_and_access_tokens_expire() {
    let h = harness_with(|c| {
        c.code_ttl = Duration::from_millis(300);
        c.access_ttl = jiff::SignedDuration::from_secs(1);
    })
    .await;
    let (verifier, challenge) = pkce();
    let code = h.code(&challenge).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, body) = h.exchange(&code, &verifier).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    let tokens = h.sign_in().await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(h.mcp_status(access).await, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.mcp_status(access).await, StatusCode::UNAUTHORIZED);
    // The refresh token still works after the access token expired.
    let (status, fresh) = h.refresh(tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.mcp_status(fresh["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_rotates_and_reuse_revokes_the_family() {
    let h = harness().await;
    let first = h.sign_in().await;
    let other_sign_in = h.sign_in().await;
    let (status, second) = h.refresh(first["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(second["refresh_token"], first["refresh_token"]);
    let second_access = second["access_token"].as_str().unwrap();
    assert_eq!(h.mcp_status(second_access).await, StatusCode::OK);

    // Replaying the rotated refresh token kills the whole family...
    let (status, body) = h.refresh(first["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(h.mcp_status(second_access).await, StatusCode::UNAUTHORIZED);
    let (status, _) = h.refresh(second["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // ...but not other sign-ins.
    assert_eq!(
        h.mcp_status(other_sign_in["access_token"].as_str().unwrap())
            .await,
        StatusCode::OK
    );
    // Another client can't use someone's refresh token.
    let (status, _) = h
        .token(&[
            ("grant_type", "refresh_token"),
            (
                "refresh_token",
                other_sign_in["refresh_token"].as_str().unwrap(),
            ),
            ("client_id", "https://other.example/client.json"),
        ])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // An access token is not a refresh token.
    let (status, _) = h
        .refresh(other_sign_in["access_token"].as_str().unwrap())
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn revoke_all_and_password_change_sign_everyone_out() {
    let h = harness().await;
    let tokens = h.sign_in().await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(h.mcp_status(access).await, StatusCode::OK);
    // As `hkc-mcp revoke-all` would, from another process.
    let other = StateFile::new(h.dir.path().join("mcp-oauth.json"));
    let (revoked, _) = super::revoke_all(&other, false).await.unwrap();
    assert_eq!(revoked, 2);
    assert_eq!(h.mcp_status(access).await, StatusCode::UNAUTHORIZED);
    let (status, _) = h.refresh(tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let tokens = h.sign_in().await;
    let n = super::set_password(&other, "a brand new passphrase", true)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        h.mcp_status(tokens["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    // The old password no longer works.
    let (_, challenge) = pkce();
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let response = h
        .submit(&request_id, &csrf, Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sign_in_attempts_are_rate_limited() {
    let h = harness().await;
    let (_, challenge) = pkce();
    let mut blocked = None;
    for attempt in 0..6 {
        let (request_id, csrf, cookie) = h.open_page(&challenge).await;
        let response = h
            .submit(&request_id, &csrf, Some(&cookie), "not the password", None)
            .await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            assert!(response.headers().contains_key(header::RETRY_AFTER));
            blocked = Some(attempt);
            break;
        }
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(blocked, Some(4), "blocked after 3 free failures and a 4th");
    // Even the right password waits.
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let response = h
        .submit(&request_id, &csrf, Some(&cookie), PASSWORD, None)
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // Without --trust-proxy, X-Forwarded-For can't dodge the limit.
    let response = h
        .submit(
            &request_id,
            &csrf,
            Some(&cookie),
            PASSWORD,
            Some("198.51.100.9"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn behind_a_trusted_proxy_limits_key_on_the_forwarded_address() {
    let h = harness_with(|c| c.trust_proxy = true).await;
    let (_, challenge) = pkce();
    for _ in 0..4 {
        let (request_id, csrf, cookie) = h.open_page(&challenge).await;
        let response = h
            .submit(
                &request_id,
                &csrf,
                Some(&cookie),
                "not the password",
                // The client-supplied first entry is ignored; the proxy's
                // (last) entry counts.
                Some("10.9.9.9, 203.0.113.5"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let blocked = h
        .submit(
            &request_id,
            &csrf,
            Some(&cookie),
            PASSWORD,
            Some("203.0.113.5"),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    let other = h
        .submit(
            &request_id,
            &csrf,
            Some(&cookie),
            PASSWORD,
            Some("10.9.9.9, 198.51.100.7"),
        )
        .await;
    assert_eq!(other.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn allowlist_restricts_clients() {
    let h = harness_with(|c| {
        c.allowed_clients = Some(vec!["https://claude.example/only.json".into()]);
    })
    .await;
    let (_, challenge) = pkce();
    let response = h.get(&h.authorize_query(&challenge, &[])).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(text(response).await.contains("HKC_MCP_ALLOWED_CLIENTS"));
}

async fn register(h: &Harness, body: Value) -> (StatusCode, Value) {
    let response = h
        .send(
            Request::post("/register")
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
    let status = response.status();
    (status, serde_json::from_str(&text(response).await).unwrap())
}

#[tokio::test]
async fn dynamic_registration_only_when_enabled() {
    let h = harness().await;
    let (status, _) = register(&h, json!({ "redirect_uris": [REDIRECT] })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut h = harness_with(|c| c.allow_dcr = true).await;
    let doc: Value =
        serde_json::from_str(&text(h.get("/.well-known/oauth-authorization-server").await).await)
            .unwrap();
    assert_eq!(
        doc["registration_endpoint"],
        "https://car.example.com/register"
    );
    for bad in [
        json!({ "redirect_uris": ["http://evil.example/cb"] }),
        json!({ "redirect_uris": [] }),
        json!({ "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic" }),
        json!({ "redirect_uris": [REDIRECT], "grant_types": ["client_credentials"] }),
    ] {
        let (status, _) = register(&h, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (status, client) = register(
        &h,
        json!({ "redirect_uris": [REDIRECT, "http://localhost:6274/cb"], "client_name": "Inspector" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(client["token_endpoint_auth_method"], "none");
    h.client_id = client["client_id"].as_str().unwrap().to_owned();
    let tokens = h.sign_in().await;
    assert_eq!(
        h.mcp_status(tokens["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn without_a_password_sign_in_is_impossible() {
    let h = harness().await;
    h.oauth
        .state
        .update(|s| s.password_hash = None)
        .await
        .unwrap();
    assert!(!h.oauth.has_password().await.unwrap());
    let (_, challenge) = pkce();
    let (request_id, csrf, cookie) = h.open_page(&challenge).await;
    let response = h.submit(&request_id, &csrf, Some(&cookie), "", None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn foreign_host_header_is_refused_at_mcp() {
    let h = harness().await;
    let tokens = h.sign_in().await;
    let response = h
        .send(
            Request::post("/mcp")
                .header(header::HOST, "attacker.example")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
                )
                .body(Body::from(initialize().to_string()))
                .unwrap(),
        )
        .await;
    assert!(response.status().is_client_error(), "{}", response.status());
}

#[test]
fn public_url_must_be_an_https_origin() {
    assert!(OAuthConfig::new("https://car.example.com").is_ok());
    assert!(OAuthConfig::new("https://car.example.com/").is_ok());
    assert!(OAuthConfig::new("http://127.0.0.1:8787").is_ok());
    for bad in [
        "http://car.example.com",
        "https://car.example.com/mcp",
        "https://u:p@car.example.com",
        "car.example.com",
    ] {
        assert!(OAuthConfig::new(bad).is_err(), "{bad}");
    }
    let c = OAuthConfig::new("https://car.example.com:8443/").unwrap();
    assert_eq!(c.issuer(), "https://car.example.com:8443");
    assert_eq!(c.resource(), "https://car.example.com:8443/mcp");
    assert_eq!(c.host(), "car.example.com:8443");
}
