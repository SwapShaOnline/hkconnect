//! OAuth Client ID Metadata Documents (CIMD).
//!
//! A client may use an `https` URL as its client id; the document at that
//! URL lists its name and redirect URIs. Fetching a URL chosen by whoever
//! opens `/authorize` is a server-side request forgery risk, so:
//!
//! - only `https` URLs with a path, no credentials, no fragment, already in
//!   canonical form;
//! - the host must resolve only to public addresses (no loopback, private,
//!   link-local, CGNAT, multicast, documentation, ...), and the connection is
//!   pinned to the checked address so DNS can't change in between;
//! - no redirects, 5 s total timeout, 64 KiB body limit;
//! - the document's `client_id` must equal the URL exactly, it must list
//!   redirect URIs, and it must be a public client (`none` auth).
//!
//! Results are cached briefly. Tests may lift the https and public-address
//! rules to use a local fake server; that switch exists only in test builds.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;
use url::{Host, Url};

/// Largest metadata document accepted.
const MAX_BODY: usize = 64 * 1024;
/// Whole-fetch time limit.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a fetched document is reused.
const CACHE_FOR: Duration = Duration::from_secs(10 * 60);
/// Most cached documents.
const MAX_CACHED: usize = 32;

/// What the server needs to know about a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Display name, if the client gave one.
    pub name: Option<String>,
    /// Exact redirect URIs allowed.
    pub redirect_uris: Vec<String>,
}

#[derive(Deserialize)]
struct Document {
    client_id: String,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

/// Fetches and caches client metadata documents.
#[derive(Debug, Default)]
pub struct Fetcher {
    cache: Mutex<HashMap<String, (Instant, ClientInfo)>>,
    #[cfg(test)]
    allow_local: bool,
}

/// Whether `client_id` is meant to be a metadata document URL.
pub fn is_url_client_id(client_id: &str) -> bool {
    client_id.starts_with("https://") || client_id.starts_with("http://")
}

impl Fetcher {
    /// A fetcher that also accepts `http` and local addresses (tests only).
    #[cfg(test)]
    pub fn allowing_local() -> Self {
        Self {
            allow_local: true,
            ..Self::default()
        }
    }

    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "the switch exists only in test builds")
    )]
    fn allow_local(&self) -> bool {
        #[cfg(test)]
        {
            self.allow_local
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Fetch and validate the document for `client_id`.
    pub async fn resolve(&self, client_id: &str) -> Result<ClientInfo, String> {
        {
            let cache = self.cache.lock().await;
            if let Some((at, info)) = cache.get(client_id)
                && at.elapsed() < CACHE_FOR
            {
                return Ok(info.clone());
            }
        }
        let info = self.fetch(client_id).await?;
        let mut cache = self.cache.lock().await;
        if cache.len() >= MAX_CACHED {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_FOR);
            if cache.len() >= MAX_CACHED {
                cache.clear();
            }
        }
        cache.insert(client_id.to_owned(), (Instant::now(), info.clone()));
        Ok(info)
    }

    async fn fetch(&self, client_id: &str) -> Result<ClientInfo, String> {
        let url = self.check_url(client_id)?;
        let host = url.host().ok_or("client id URL has no host")?;
        let port = url
            .port_or_known_default()
            .ok_or("client id URL has no port")?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .user_agent(concat!("hkc-mcp/", env!("CARGO_PKG_VERSION")));
        match host {
            Host::Ipv4(ip) => self.check_ip(IpAddr::V4(ip))?,
            Host::Ipv6(ip) => self.check_ip(IpAddr::V6(ip))?,
            Host::Domain(domain) => {
                let addrs: Vec<SocketAddr> =
                    tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((domain, port)))
                        .await
                        .map_err(|_| "timed out resolving the client id host".to_owned())?
                        .map_err(|e| format!("could not resolve the client id host: {e}"))?
                        .collect();
                if addrs.is_empty() {
                    return Err("the client id host has no addresses".into());
                }
                for addr in &addrs {
                    self.check_ip(addr.ip())?;
                }
                // Connect to the address that was checked, not a fresh lookup.
                builder = builder.resolve(domain, addrs[0]);
            }
        }
        let http = builder
            .build()
            .map_err(|e| format!("could not build HTTP client: {e}"))?;
        let mut response = http
            .get(url.clone())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("could not fetch the client metadata document: {e}"))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "the client metadata document answered HTTP {} (redirects are not followed)",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err("the client metadata document is too large".into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("could not read the client metadata document: {e}"))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err("the client metadata document is too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        let doc: Document = serde_json::from_slice(&body)
            .map_err(|e| format!("the client metadata document is not valid: {e}"))?;
        validate(client_id, doc)
    }

    fn check_url(&self, client_id: &str) -> Result<Url, String> {
        let url = Url::parse(client_id).map_err(|_| "client id is not a valid URL".to_owned())?;
        let scheme_ok = url.scheme() == "https" || (self.allow_local() && url.scheme() == "http");
        if !scheme_ok {
            return Err("client id URLs must use https".into());
        }
        if url.as_str() != client_id {
            return Err("client id URL is not in canonical form".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("client id URL must not contain credentials".into());
        }
        if url.fragment().is_some() {
            return Err("client id URL must not contain a fragment".into());
        }
        if url.path() == "/" || url.path().is_empty() {
            return Err("client id URL must have a path".into());
        }
        Ok(url)
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), String> {
        if self.allow_local() || is_public(ip) {
            Ok(())
        } else {
            Err(format!(
                "the client id host resolves to a non-public address ({ip}); refusing to fetch"
            ))
        }
    }
}

fn validate(client_id: &str, doc: Document) -> Result<ClientInfo, String> {
    if doc.client_id != client_id {
        return Err("the metadata document's client_id does not match its URL".into());
    }
    if doc.redirect_uris.is_empty() {
        return Err("the metadata document lists no redirect_uris".into());
    }
    if doc.redirect_uris.len() > 20 || doc.redirect_uris.iter().any(|u| u.len() > 2048) {
        return Err("the metadata document's redirect_uris are too many or too long".into());
    }
    match doc.token_endpoint_auth_method.as_deref() {
        None | Some("none") => {}
        Some(other) => {
            return Err(format!(
                "client authentication method {other:?} is not supported (public clients only)"
            ));
        }
    }
    let name = doc
        .client_name
        .map(|n| n.chars().take(100).collect::<String>())
        .filter(|n| !n.trim().is_empty());
    Ok(ClientInfo {
        name,
        redirect_uris: doc.redirect_uris,
    })
}

/// Whether an address is globally routable (not loopback, private, ...).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64: may map to private v4
        || (s[0] == 0x2002) // 6to4: embeds a v4 address
        || s[..6] == [0; 6]) // v4-compatible ::a.b.c.d
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn address_classes() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "224.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in [
            "8.8.8.8",
            "160.79.104.10",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn real_fetcher_refuses_http_private_and_odd_urls_without_connecting() {
        let f = Fetcher::default();
        for (id, why) in [
            ("http://example.com/client.json", "https"),
            ("https://127.0.0.1/client.json", "non-public"),
            ("https://10.0.0.7/client.json", "non-public"),
            ("https://[::1]/client.json", "non-public"),
            ("https://localhost/client.json", "non-public"),
            ("https://example.com/", "path"),
            ("https://user:pw@example.com/c", "credentials"),
            ("https://example.com/c#frag", "fragment"),
            ("https://EXAMPLE.com/c", "canonical"),
        ] {
            let err = f.resolve(id).await.unwrap_err();
            assert!(err.contains(why), "{id}: {err}");
        }
    }

    async fn serve(body: ResponseTemplate) -> (MockServer, String) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client.json"))
            .respond_with(body)
            .mount(&server)
            .await;
        let id = format!("{}/client.json", server.uri());
        (server, id)
    }

    #[tokio::test]
    async fn valid_document_is_accepted_and_cached() {
        let server = MockServer::start().await;
        let id = format!("{}/client.json", server.uri());
        Mock::given(method("GET"))
            .and(path("/client.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": id,
                "client_name": "Test Client",
                "redirect_uris": ["https://app.example/callback"],
                "token_endpoint_auth_method": "none",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let f = Fetcher::allowing_local();
        let info = f.resolve(&id).await.unwrap();
        assert_eq!(info.name.as_deref(), Some("Test Client"));
        assert_eq!(info.redirect_uris, ["https://app.example/callback"]);
        // Second call is served from the cache (expect(1) above).
        assert_eq!(f.resolve(&id).await.unwrap(), info);
    }

    #[tokio::test]
    async fn mismatched_client_id_is_rejected() {
        let (_server, id) = serve(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "https://evil.example/client.json",
            "redirect_uris": ["https://app.example/callback"],
        })))
        .await;
        let err = Fetcher::allowing_local().resolve(&id).await.unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[tokio::test]
    async fn oversized_redirecting_and_confidential_documents_are_rejected() {
        let big = "x".repeat(MAX_BODY + 1);
        let (_s1, id) = serve(ResponseTemplate::new(200).set_body_string(big)).await;
        let err = Fetcher::allowing_local().resolve(&id).await.unwrap_err();
        assert!(err.contains("too large"), "{err}");

        let (_s2, id) =
            serve(ResponseTemplate::new(302).insert_header("location", "http://169.254.169.254/"))
                .await;
        let err = Fetcher::allowing_local().resolve(&id).await.unwrap_err();
        assert!(err.contains("302"), "{err}");

        let server = MockServer::start().await;
        let id = format!("{}/client.json", server.uri());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": id,
                "redirect_uris": ["https://app.example/cb"],
                "token_endpoint_auth_method": "private_key_jwt",
            })))
            .mount(&server)
            .await;
        let err = Fetcher::allowing_local().resolve(&id).await.unwrap_err();
        assert!(err.contains("public clients only"), "{err}");

        let (_s3, id) = serve(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "placeholder",
        })))
        .await;
        assert!(Fetcher::allowing_local().resolve(&id).await.is_err());
    }
}
