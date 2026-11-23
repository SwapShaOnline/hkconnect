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
.muted{color:#666;font-size:.9rem}";

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
    /// Who is asking (client name or id).
    pub client: &'a str,
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
pub fn sign_in(status: StatusCode, s: &SignIn<'_>) -> Response {
    let error = s
        .error
        .map(|e| format!("<p class=\"err\">{}</p>", escape(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>Allow access to your car?</h1>\
         <div class=\"box\">\
         <p><strong>{client}</strong> wants to read and control your car through hkc-mcp.</p>\
         <p class=\"muted\">After you sign in, your browser returns to <code>{host}</code>. \
         Only continue if you started this from Claude (or another app you trust).</p>\
         {error}\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label for=\"password\">hkc-mcp password</label>\
         <input id=\"password\" type=\"password\" name=\"password\" autocomplete=\"current-password\" required autofocus>\
         <button type=\"submit\">Allow</button>\
         </form></div>\
         <p class=\"muted\">To refuse, close this page.</p>",
        client = escape(s.client),
        host = escape(s.redirect_host),
        request = escape(s.request),
        csrf = escape(s.csrf),
    );
    html(status, "Sign in to hkc-mcp", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        assert_eq!(
            escape("<script>\"x\" & 'y'</script>"),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }
}
