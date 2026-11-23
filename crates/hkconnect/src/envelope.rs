//! Decoding the API's response wrapper.
//!
//! Vehicle endpoints wrap results as
//! `{"retCode": "S"|"F", "resCode": "0000", "resMsg": <payload>, "msgId": "…"}`.
//! Auth endpoints return bare OAuth-style JSON instead. Both go through
//! [`parse`], so every call site gets the same error mapping.
//!
//! # Classification
//!
//! 1. An envelope whose `retCode` is anything but `"S"` is a failure, mapped
//!    from its `resCode` by [`Error::from_res_code`]. An `"S"` envelope is a
//!    success only when the HTTP status is 2xx too.
//! 2. Anything else with a non-2xx HTTP status is classified by status first:
//!    401/403 → [`Error::Authentication`], 429 → [`Error::RateLimited`],
//!    502/503/504 → [`Error::ServiceUnavailable`] (a gateway timeout is not
//!    evidence that the *car* timed out, so it is not [`Error::CarTimeout`]),
//!    anything else → [`Error::Api`] with the status as the code (or
//!    [`Error::UnexpectedResponse`] when the body is not JSON).
//! 3. A 2xx bare JSON body with a non-null `"error"` value is
//!    [`Error::Authentication`] if the message looks like an auth failure,
//!    else [`Error::Api`]. `"error": null` is not an error.

use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{Error, Result};

#[derive(Debug)]
pub(crate) struct Reply<T> {
    pub body: T,
    /// Present on commands; used to look up the command's result later.
    pub msg_id: Option<String>,
}

/// A failed reply, plus whether the server's answer *proves* the request was
/// rejected before it was acted on.
///
/// Only such rejections make it safe to resend a non-idempotent command: the
/// HTTP status was 401/403, or the envelope carried `resCode` 7501 (session
/// rejected) or 4002 (device id rejected). Gateway errors, timeouts, rate
/// limits and unknown failures leave it unknown whether the car got the
/// command, so they never qualify.
#[derive(Debug)]
pub(crate) struct Rejection {
    pub error: Error,
    pub not_executed: bool,
}

impl From<Error> for Rejection {
    fn from(error: Error) -> Self {
        Self {
            error,
            not_executed: false,
        }
    }
}

impl From<reqwest::Error> for Rejection {
    fn from(e: reqwest::Error) -> Self {
        Error::http(e).into()
    }
}

/// Decode a reply, discarding the [`Rejection::not_executed`] detail.
pub(crate) fn parse<T: DeserializeOwned>(status: StatusCode, bytes: &[u8]) -> Result<Reply<T>> {
    parse_classified(status, bytes).map_err(|r| r.error)
}

/// Decode a reply, keeping whether a failure proves the request was not
/// acted on.
pub(crate) fn parse_classified<T: DeserializeOwned>(
    status: StatusCode,
    bytes: &[u8],
) -> Result<Reply<T>, Rejection> {
    let auth_status = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
    let fail = |error: Error, not_executed: bool| Rejection {
        error,
        not_executed,
    };

    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        let snippet = snippet(bytes);
        return Err(match classify_status(status, &snippet) {
            Some(error) => fail(error, auth_status),
            None => fail(
                Error::UnexpectedResponse(format!("HTTP {status}: {snippet}")),
                false,
            ),
        });
    };

    if let Some(ret_code) = value.get("retCode").and_then(Value::as_str) {
        if ret_code != "S" {
            // Normally a string; a number (7501) is read as the same code.
            let code = match value.get("resCode") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => "?".to_owned(),
            };
            let code = code.as_str();
            let message = match value.get("resMsg") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let proven = auth_status || matches!(code, "7501" | "4002");
            return Err(fail(Error::from_res_code(code, message), proven));
        }
        if !status.is_success() {
            let error = classify_status(status, &value.to_string())
                .unwrap_or_else(|| http_api_error(status, value.to_string()));
            return Err(fail(error, auth_status));
        }
        // Normally a string; accept a number too, compared as text later.
        let msg_id = match value.get("msgId") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let payload = value.get("resMsg").cloned().unwrap_or(Value::Null);
        return Ok(Reply {
            body: decode(payload)?,
            msg_id,
        });
    }

    let oauth_error = value.get("error").filter(|v| !v.is_null()).map(|error| {
        value
            .get("error_description")
            .filter(|v| !v.is_null())
            .unwrap_or(error)
            .as_str()
            .map_or_else(|| error.to_string(), str::to_owned)
    });

    if !status.is_success() {
        let message = oauth_error.unwrap_or_else(|| value.to_string());
        let error =
            classify_status(status, &message).unwrap_or_else(|| http_api_error(status, message));
        return Err(fail(error, auth_status));
    }

    if let Some(message) = oauth_error {
        let error = if looks_like_auth_failure(&message) {
            Error::Authentication(message)
        } else {
            http_api_error(status, message)
        };
        return Err(fail(error, false));
    }

    Ok(Reply {
        body: decode(value)?,
        msg_id: None,
    })
}

/// Errors decided by the HTTP status alone; `None` for other statuses.
fn classify_status(status: StatusCode, message: &str) -> Option<Error> {
    match status.as_u16() {
        401 | 403 => Some(Error::Authentication(if message.is_empty() {
            format!("HTTP {status}")
        } else {
            message.to_owned()
        })),
        429 => Some(Error::RateLimited),
        502..=504 => Some(Error::ServiceUnavailable),
        _ => None,
    }
}

fn http_api_error(status: StatusCode, message: String) -> Error {
    Error::Api {
        code: status.as_u16().to_string(),
        message,
    }
}

fn looks_like_auth_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "auth",
        "token",
        "expired",
        "invalid_grant",
        "invalid_client",
        "credential",
        "password",
        "login",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

fn snippet(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned()
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::UnexpectedResponse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T: DeserializeOwned>(json: &str) -> Result<Reply<T>> {
        parse(StatusCode::OK, json.as_bytes())
    }

    #[test]
    fn success_unwraps_payload_and_msg_id() {
        let r: Reply<Value> =
            run(r#"{"retCode":"S","resCode":"0000","resMsg":{"a":1},"msgId":"m1"}"#).unwrap();
        assert_eq!(r.body["a"], 1);
        assert_eq!(r.msg_id.as_deref(), Some("m1"));
    }

    #[test]
    fn failure_codes_map_to_errors() {
        let cases = [
            ("5091", "RateLimited"),
            ("4002", "InvalidDeviceId"),
            ("4004", "DuplicateRequest"),
            ("9999", "CarTimeout"),
        ];
        for (code, expected) in cases {
            let json = format!(r#"{{"retCode":"F","resCode":"{code}","resMsg":"x"}}"#);
            let err = run::<Value>(&json).unwrap_err();
            assert!(
                format!("{err:?}").starts_with(expected),
                "{code} -> {err:?}"
            );
        }
    }

    #[test]
    fn unknown_failure_keeps_code_and_message() {
        let err = run::<Value>(r#"{"retCode":"F","resCode":"1234","resMsg":"Nope"}"#).unwrap_err();
        assert!(
            matches!(err, Error::Api { ref code, ref message } if code == "1234" && message == "Nope")
        );
    }

    #[test]
    fn oauth_error_is_authentication() {
        let err = run::<Value>(r#"{"error":"Key not authorized: Token is expired"}"#).unwrap_err();
        assert!(matches!(err, Error::Authentication(m) if m.contains("expired")));
    }

    #[test]
    fn non_json_is_reported_with_status() {
        let err = parse::<Value>(StatusCode::INTERNAL_SERVER_ERROR, b"<html>").unwrap_err();
        assert!(matches!(err, Error::UnexpectedResponse(m) if m.contains("500")));
        let err = parse::<Value>(StatusCode::BAD_GATEWAY, b"<html>").unwrap_err();
        assert!(matches!(err, Error::ServiceUnavailable), "{err:?}");
    }

    fn at<T: DeserializeOwned>(status: u16, json: &str) -> Result<Reply<T>, Rejection> {
        parse_classified(StatusCode::from_u16(status).unwrap(), json.as_bytes())
    }

    #[test]
    fn gateway_errors_are_classified_by_status_not_as_auth() {
        let gateway = r#"{"error":"Upstream timed out"}"#;
        for (status, check) in [
            (
                504,
                (|e| matches!(e, Error::ServiceUnavailable)) as fn(&Error) -> bool,
            ),
            (502, |e| matches!(e, Error::ServiceUnavailable)),
            (503, |e| matches!(e, Error::ServiceUnavailable)),
            (429, |e| matches!(e, Error::RateLimited)),
            (
                500,
                |e| matches!(e, Error::Api { code, .. } if code == "500"),
            ),
            (
                400,
                |e| matches!(e, Error::Api { code, .. } if code == "400"),
            ),
        ] {
            let r = at::<Value>(status, gateway).unwrap_err();
            assert!(check(&r.error), "{status} -> {:?}", r.error);
            assert!(!r.not_executed, "{status} must not allow a resend");
        }
    }

    #[test]
    fn http_401_and_403_are_recoverable_auth_failures() {
        for body in ["", "<html>", "{}", r#"{"error":"Key not authorized"}"#] {
            for status in [401, 403] {
                let r = at::<Value>(status, body).unwrap_err();
                assert!(matches!(r.error, Error::Authentication(_)), "{r:?}");
                assert!(r.not_executed);
            }
        }
    }

    #[test]
    fn only_retcode_s_is_success() {
        let r = at::<Value>(200, r#"{"retCode":"X","resCode":"1234","resMsg":"?"}"#).unwrap_err();
        assert!(matches!(r.error, Error::Api { ref code, .. } if code == "1234"));
        let r = at::<Value>(500, r#"{"retCode":"S","resCode":"0000","resMsg":{}}"#).unwrap_err();
        assert!(matches!(r.error, Error::Api { ref code, .. } if code == "500"));
    }

    #[test]
    fn session_and_device_rejections_prove_non_execution() {
        for (code, proven) in [
            ("7501", true),
            ("4002", true),
            ("9999", false),
            ("5031", false),
        ] {
            let json = format!(r#"{{"retCode":"F","resCode":"{code}","resMsg":"x"}}"#);
            assert_eq!(
                at::<Value>(200, &json).unwrap_err().not_executed,
                proven,
                "{code}"
            );
        }
    }

    #[test]
    fn numeric_res_codes_are_mapped_like_strings() {
        for (code, proven) in [(7501, true), (4002, true), (5091, false)] {
            let json = format!(r#"{{"retCode":"F","resCode":{code},"resMsg":"x"}}"#);
            let r = at::<Value>(200, &json).unwrap_err();
            assert_eq!(r.not_executed, proven, "{code}");
            let expected = match code {
                7501 => matches!(r.error, Error::Authentication(_)),
                4002 => matches!(r.error, Error::InvalidDeviceId),
                _ => matches!(r.error, Error::RateLimited),
            };
            assert!(expected, "{code} -> {:?}", r.error);
        }
        let r = at::<Value>(200, r#"{"retCode":"F","resCode":[1],"resMsg":"x"}"#).unwrap_err();
        assert!(matches!(r.error, Error::Api { ref code, .. } if code == "?"));
    }

    #[test]
    fn null_error_is_not_an_error() {
        let r: Reply<Value> = run(r#"{"error":null,"access_token":"a"}"#).unwrap();
        assert_eq!(r.body["access_token"], "a");
    }

    #[test]
    fn non_auth_error_string_on_2xx_is_api() {
        let err = run::<Value>(r#"{"error":"something odd"}"#).unwrap_err();
        assert!(
            matches!(err, Error::Api { ref code, .. } if code == "200"),
            "{err:?}"
        );
    }
}
