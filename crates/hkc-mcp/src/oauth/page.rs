//! The sign-in page and error pages. Plain HTML, no scripts, no external
//! resources; every dynamic value is escaped.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};

/// Escape text for HTML element content and quoted attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:28rem;margin:3rem auto;\
padding:0 1rem;line-height:1.5;color:#222;background:#fafafa}\
h1{font-size:1.3rem}code{word-break:break-all}\
.box{background:#fff;border:1px solid #ddd;border-radius:8px;padding:1rem 1.25rem}\
input[type=password]{width:100%;padding:.6rem;font-size:1rem;box-sizing:border-box;margin:.25rem 0 1rem}\
button{padding:.6rem 1.2rem;font-size:1rem}.err{color:#b00020;font-weight:600}\
.muted{color:#666;font-size:.9rem}\
.host{font-family:ui-monospace,monospace;font-size:1.1rem;word-break:break-all}";

/// A complete HTML response with defensive headers.
pub fn html(status: StatusCode, title: &str, body: &str) -> Response {
    let doc = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        escape(title)
    );
    let mut response = (status, doc).into_response();
    secure_headers(response.headers_mut());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// Headers for every OAuth page: no caching, no framing, no sniffing, no
/// referrer (the URL carries the authorization request).
pub fn secure_headers(headers: &mut axum::http::HeaderMap) {
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::PRAGMA, "no-cache"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// An error page. Never redirects anywhere.
pub fn error(status: StatusCode, message: &str) -> Response {
    html(
        status,
        "Sign-in problem",
        &format!(
            "<h1>Can't continue</h1><div class=\"box\"><p>{}</p></div>",
            escape(message)
        ),
    )
}

/// What the sign-in form shows and posts back.
#[derive(Debug)]
pub struct SignIn<'a> {
    /// The client id.
    pub client_id: &'a str,
    /// The host of a URL client id: who the client really is.
    pub client_host: Option<&'a str>,
    /// The name the client gave itself (unverified; shown de-emphasised).
    pub client_name: Option<&'a str>,
    /// Where the browser goes after sign-in (shown as a host).
    pub redirect_host: &'a str,
    /// The pending request id.
    pub request: &'a str,
    /// The CSRF token (also in a cookie).
    pub csrf: &'a str,
    /// An error from the previous attempt.
    pub error: Option<&'a str>,
}

/// The password form.
///
/// The client is identified by the host of its client id URL, which the
/// server fetched; the name it gives itself is anyone's choice, so it is
/// shown only as "calls itself". If the client id host and the return host
/// differ, a warning says so.
pub fn sign_in(status: StatusCode, s: &SignIn<'_>) -> Response {
    let error = s
        .error
        .map(|e| format!("<p class=\"err\">{}</p>", escape(e)))
        .unwrap_or_default();
    let who = match s.client_host {
        Some(h) => format!(
            "<p>An app from <strong class=\"host\">{}</strong> wants to read and control \
             your car through hkc-mcp.</p>",
            escape(h)
        ),
        None => format!(
            "<p>A dynamically registered app (<code>{}</code>) wants to read and control \
             your car through hkc-mcp.</p>",
            escape(s.client_id)
        ),
    };
    let name = s
        .client_name
        .map(|n| {
            format!(
                "<p class=\"muted\">It calls itself \u{201c}{}\u{201d}. Any app can pick any \
                 name; trust the addresses, not the name.</p>",
                escape(n)
            )
        })
        .unwrap_or_default();
    let mismatch = match s.client_host {
        Some(h) if !h.eq_ignore_ascii_case(s.redirect_host) => format!(
            "<p class=\"err\">Careful: the app is from <code>{}</code> but sends your \
             browser back to <code>{}</code>. Unless you expected exactly this, close \
             this page.</p>",
            escape(h),
            escape(s.redirect_host)
        ),
        _ => String::new(),
    };
    let body = format!(
        "<h1>Allow access to your car?</h1>\
         <div class=\"box\">\
         {who}{name}\
         <p>After you sign in, your browser returns to \
         <strong class=\"host\">{host}</strong>.</p>\
         {mismatch}\
         <p class=\"muted\">Only continue if you started this yourself from Claude (or \
         another app you trust) just now.</p>\
         {error}\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label for=\"password\">hkc-mcp password</label>\
         <input id=\"password\" type=\"password\" name=\"password\" autocomplete=\"current-password\" required autofocus>\
         <button type=\"submit\">Allow</button>\
         </form></div>\
         <p class=\"muted\">To refuse, close this page.</p>",
        host = escape(s.redirect_host),
        request = escape(s.request),
        csrf = escape(s.csrf),
    );
    html(status, "Sign in to hkc-mcp", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(client_host: Option<&str>, redirect_host: &str) -> String {
        let r = sign_in(
            StatusCode::OK,
            &SignIn {
                client_id: "https://evil.example/c.json",
                client_host,
                client_name: Some("Claude"),
                redirect_host,
                request: "r",
                csrf: "c",
                error: None,
            },
        );
        let body = r.into_body();
        let bytes = futures_body(body);
        String::from_utf8(bytes).unwrap()
    }

    fn futures_body(body: axum::body::Body) -> Vec<u8> {
        use http_body_util::BodyExt as _;
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(body.collect())
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[test]
    fn shows_the_client_host_and_warns_on_mismatch() {
        let html = page(Some("evil.example"), "claude.ai");
        assert!(html.contains("<strong class=\"host\">evil.example</strong>"));
        assert!(html.contains("calls itself \u{201c}Claude\u{201d}"));
        assert!(!html.contains("<strong>Claude</strong>"));
        assert!(html.contains("Careful"));
        let html = page(Some("claude.ai"), "claude.ai");
        assert!(!html.contains("Careful"));
        let html = page(None, "localhost");
        assert!(html.contains("dynamically registered"));
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(
            escape("<script>\"x\" & 'y'</script>"),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }
}
