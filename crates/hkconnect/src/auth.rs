//! Login, token refresh, and device registration.
//!
//! Login is a four-step dance (≈3.5 s):
//! 1. register a "device" (the server's id for this app install),
//! 2. open an OAuth authorize page to get a session cookie,
//! 3. sign in with email + password to get an authorization code,
//! 4. swap the code for access and refresh tokens.

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::config::Config;
use crate::de::flexible_int;
use crate::envelope;
use crate::session::{Credentials, Session};
use crate::stamp::stamp_now;
use crate::{Error, Result};

const SERVICE_ID: &str = "ccsp-service-id";
const APP_ID: &str = "ccsp-application-id";

#[derive(Deserialize)]
struct DeviceRegistration {
    #[serde(rename = "deviceId")]
    device_id: String,
}

#[derive(Deserialize)]
struct SignIn {
    #[serde(rename = "redirectUrl")]
    redirect_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    token_type: String,
    access_token: String,
    refresh_token: Option<String>,
    /// Seconds; some servers send it as a string (`"3600"`).
    #[serde(default, deserialize_with = "flexible_int")]
    expires_in: Option<i64>,
}

/// Shortest lifetime accepted from the server for a token (see
/// [`expiry_from_now`]).
const MIN_TOKEN_LIFETIME: SignedDuration = SignedDuration::from_secs(60);
/// Longest lifetime accepted from the server for a token: 30 days.
const MAX_TOKEN_LIFETIME: SignedDuration = SignedDuration::from_secs(30 * 24 * 60 * 60);

/// When a token that the server says lives `secs` seconds expires.
///
/// The value is clamped to 60 s ..= 30 days so a hostile or garbled
/// `expires_in` / `expiresTime` can neither overflow the timestamp (which
/// would panic) nor pin a token for years. Zero or negative values become
/// 60 s: the token is used for the request that obtained it and refreshed
/// soon after.
pub(crate) fn expiry_from_now(secs: i64) -> Timestamp {
    let lifetime = SignedDuration::from_secs(secs).clamp(MIN_TOKEN_LIFETIME, MAX_TOKEN_LIFETIME);
    Timestamp::now()
        .checked_add(lifetime)
        .unwrap_or_else(|_| Timestamp::now())
}

async fn send<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
) -> Result<envelope::Reply<T>> {
    let resp = req.send().await.map_err(Error::http)?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(Error::http)?;
    envelope::parse(status, &bytes)
}

/// Like [`send`], for the sign-in and token endpoints: a 4xx rejection there
/// (other than 429) means the credentials or tokens were refused, so it is
/// reported as [`Error::Authentication`].
async fn send_oauth<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
) -> Result<envelope::Reply<T>> {
    send(req).await.map_err(|e| match e {
        Error::Api { code, message } if is_http_4xx(&code) => Error::Authentication(message),
        other => other,
    })
}

/// Whether an [`Error::Api`] code is an HTTP 4xx status (429 is already
/// [`Error::RateLimited`]).
pub(crate) fn is_http_4xx(code: &str) -> bool {
    code.len() == 3 && code.starts_with('4')
}

pub(crate) async fn register_device(http: &reqwest::Client, cfg: &Config) -> Result<String> {
    // The server expects a push-notification registration id; we never
    // receive pushes, so a random one is fine.
    let push_id = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let body = json!({
        "pushRegId": push_id,
        "pushType": cfg.push_type,
        "uuid": uuid::Uuid::new_v4().to_string(),
    });
    let reply: envelope::Reply<DeviceRegistration> = send(
        http.post(cfg.url("/api/v1/spa/notifications/register"))
            .header(SERVICE_ID, cfg.service_id)
            .header(APP_ID, cfg.app_id)
            .header("Stamp", stamp_now(cfg.app_id, &cfg.cfb))
            .json(&body),
    )
    .await?;
    tracing::debug!("registered device");
    Ok(reply.body.device_id)
}

pub(crate) async fn login(
    http: &reqwest::Client,
    cfg: &Config,
    creds: &Credentials,
) -> Result<Session> {
    let device_id = register_device(http, cfg).await?;
    let redirect_uri = cfg.url("/api/v1/user/oauth2/redirect").to_string();

    // Step 2: sets the session cookie that sign-in requires.
    http.get(cfg.url("/api/v1/user/oauth2/authorize"))
        .query(&[
            ("response_type", "code"),
            ("state", "test"),
            ("client_id", cfg.service_id),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await
        // Redirects are not followed (see `Client::new`); a 3xx here still
        // sets the cookie, and `error_for_status` only rejects 4xx/5xx.
        .and_then(reqwest::Response::error_for_status)
        .map_err(Error::http)?;

    // Step 3.
    let sign_in: envelope::Reply<SignIn> =
        send_oauth(http.post(cfg.url("/api/v1/user/signin")).json(&json!({
            "email": creds.username(),
            "password": creds.password(),
        })))
        .await?;
    let code = sign_in
        .body
        .redirect_url
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.into_owned())
        })
        .ok_or_else(|| Error::Authentication("wrong email or password".into()))?;

    // Step 4.
    let tokens: envelope::Reply<TokenResponse> = send_oauth(
        http.post(cfg.url("/api/v1/user/oauth2/token"))
            .header("Authorization", cfg.basic_auth)
            .header("Stamp", stamp_now(cfg.app_id, &cfg.cfb))
            .form(&[
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri.as_str()),
                ("code", code.as_str()),
            ]),
    )
    .await?;
    tracing::debug!("logged in");
    session_from(tokens.body, None, device_id)
}

pub(crate) async fn refresh(
    http: &reqwest::Client,
    cfg: &Config,
    old: &Session,
) -> Result<Session> {
    let tokens: envelope::Reply<TokenResponse> = send_oauth(
        http.post(cfg.url("/api/v1/user/oauth2/token"))
            .header("Authorization", cfg.basic_auth)
            .header("Stamp", stamp_now(cfg.app_id, &cfg.cfb))
            .form(&[
                ("grant_type", "refresh_token"),
                // Sent verbatim: a "Bearer " prefix is rejected here.
                ("refresh_token", old.refresh_token.as_str()),
            ]),
    )
    .await?;
    tracing::debug!("refreshed access token");
    session_from(tokens.body, Some(&old.refresh_token), old.device_id.clone())
}

fn session_from(
    t: TokenResponse,
    previous_refresh: Option<&str>,
    device_id: String,
) -> Result<Session> {
    let refresh_token = t
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::UnexpectedResponse("token response had no refresh token".into()))?;
    Ok(Session::new(
        format!("{} {}", t.token_type, t.access_token),
        refresh_token,
        expiry_from_now(t.expires_in.unwrap_or(3600)),
        device_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_clamped_and_never_panics() {
        let now = Timestamp::now();
        for secs in [i64::MAX, i64::MIN, 0, -5, 10] {
            let at = expiry_from_now(secs);
            assert!(at > now && at <= now + MAX_TOKEN_LIFETIME + SignedDuration::from_secs(5));
        }
        let hour = expiry_from_now(3600);
        assert!(hour > now + SignedDuration::from_secs(3590));
    }
}
