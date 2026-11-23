use std::fmt;
use std::sync::Arc;

/// Everything that can go wrong talking to the car.
///
/// Where the server reports a failure with a result code (`resCode`), the
/// code is mapped to a specific variant; unrecognised codes become
/// [`Error::Api`].
///
/// When a remote command fails, [`Error::may_have_been_received`] says
/// whether the car might still act on it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The account or session was rejected: server code 7501, HTTP 401/403,
    /// an OAuth error from the login endpoints, or a sign-in that returned
    /// no authorization code. The string is the server's message.
    ///
    /// When a request is rejected this way, the client logs in again and
    /// resends it once before returning this, but only if resending is safe:
    /// always for reads, and for remote commands only when the rejection
    /// proves the command was not acted on (HTTP 401/403 or code 7501). A
    /// failed login is returned at once.
    #[error("login failed: {0}")]
    Authentication(String),
    /// The server did not return a control token for the PIN given to
    /// [`Credentials::with_pin`](crate::Credentials::with_pin): it answered
    /// the PIN request without a token, with an HTTP 4xx status, or with
    /// HTTP 401/403 right after a fresh login.
    ///
    /// The [`Client`](crate::Client) remembers a rejected PIN for the rest
    /// of its life and never submits it again: every later command that
    /// needs the PIN fails with this error at once, without contacting the
    /// server, because repeated wrong PINs can block remote control on the
    /// account. Build a new `Client` with the right PIN to try again.
    #[error("the PIN was rejected; check it matches the one in the app")]
    InvalidPin,
    /// The command needs a PIN, but none was given with
    /// [`Credentials::with_pin`](crate::Credentials::with_pin).
    #[error("a PIN is required for this command")]
    PinRequired,
    /// Server code 4002: the registered device id was not accepted. The
    /// client re-registers and resends the request once before returning
    /// this (the code proves the request was not acted on, so this applies
    /// to remote commands too).
    #[error("the server rejected this app's device registration")]
    InvalidDeviceId,
    /// Server code 4004: another command for this car is still in progress.
    #[error("a previous command is still in progress; try again shortly")]
    DuplicateRequest,
    /// Server code 4005: the car does not support this command.
    #[error("this vehicle does not support that command")]
    UnsupportedCommand,
    /// The car uses the newer "CCS2" protocol, which this crate does not
    /// support for this call. Returned before any request is sent.
    #[error("{0}")]
    UnsupportedVehicle(&'static str),
    /// Server code 4081 or 9999: the car did not answer in time.
    #[error("the car did not respond in time")]
    CarTimeout,
    /// The service is temporarily down: server code 5031, or HTTP 502, 503
    /// or 504 from the server or a gateway in front of it. A gateway timeout
    /// says nothing about whether the car got a command, so it is reported
    /// here rather than as [`Error::CarTimeout`].
    #[error("the service is temporarily unavailable")]
    ServiceUnavailable,
    /// Server code 5091 or HTTP 429: too many requests for this account;
    /// wait before retrying.
    #[error("too many requests; Hyundai/Kia servers are rate limiting this account")]
    RateLimited,
    /// Server code 5921: the server has nothing to return.
    #[error("the server has no data for this request")]
    NoData,
    /// Any other failure. For an enveloped reply, `code` is the server's
    /// `resCode`; for an unwrapped non-2xx reply, it is the HTTP status.
    #[error("server error {code}: {message}")]
    Api {
        /// The server result code, or the HTTP status code.
        code: String,
        /// The server's message, or the raw response body.
        message: String,
    },
    /// The response was not valid JSON, did not match the expected shape,
    /// or lacked a required field (such as a command's message id).
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
    /// A temperature outside 14.0–29.5 °C or not a multiple of 0.5 was
    /// passed to [`Celsius::new`](crate::Celsius::new).
    #[error("invalid temperature {0}: use 14.0 to 29.5 °C in 0.5 steps")]
    InvalidTemperature(f32),
    /// A remote start duration outside 1–10 minutes was passed to
    /// [`ClimateOptions::with_duration_minutes`](crate::ClimateOptions::with_duration_minutes).
    #[error("invalid duration {0} min: use 1 to 10")]
    InvalidDuration(u8),
    /// A month outside 1–12 was passed to
    /// [`Client::trips_in_month`](crate::Client::trips_in_month).
    #[error("invalid month {0}: use 1 to 12")]
    InvalidMonth(i8),
    /// Coordinates outside -90..=90 (latitude) or -180..=180 (longitude), or
    /// not finite, were passed to [`Destination::new`](crate::Destination::new).
    #[error(
        "invalid coordinates {latitude}, {longitude}: latitude is -90 to 90, longitude -180 to 180"
    )]
    InvalidCoordinates {
        /// The latitude given, in degrees.
        latitude: f64,
        /// The longitude given, in degrees.
        longitude: f64,
    },
    /// The car's [`Vehicle::id`](crate::Vehicle::id) cannot be used in a
    /// request path: it is empty, or contains something other than ASCII
    /// letters, digits, `-`, `_` and `.` (or contains `..`). Returned before
    /// any request is sent.
    #[error(
        "invalid vehicle id: expected only ASCII letters, digits, '-', '_' and '.', and no '..'"
    )]
    InvalidVehicleId,
    /// The HTTP request failed: connecting, TLS, a timeout, or reading the
    /// body. The [`HttpError`] (available as the error's
    /// [`source`](std::error::Error::source)) says what went wrong.
    #[error("network error")]
    Http(#[source] HttpError),
}

impl Error {
    /// Map a failed envelope's `resCode` / `resMsg` to an error.
    pub(crate) fn from_res_code(code: &str, message: impl fmt::Display) -> Self {
        match code {
            "7501" => Self::Authentication(message.to_string()),
            "4002" => Self::InvalidDeviceId,
            "4004" => Self::DuplicateRequest,
            "4005" => Self::UnsupportedCommand,
            "4081" | "9999" => Self::CarTimeout,
            "5031" => Self::ServiceUnavailable,
            "5091" => Self::RateLimited,
            "5921" => Self::NoData,
            _ => Self::Api {
                code: code.to_owned(),
                message: message.to_string(),
            },
        }
    }

    /// Wrap a transport error. The URL is dropped from it: request paths
    /// contain the vehicle id, which should not end up in logs or error
    /// reports.
    pub(crate) fn http(e: reqwest::Error) -> Self {
        Self::Http(HttpError {
            inner: Arc::new(e.without_url()),
            before_command: false,
        })
    }

    /// Record that this error happened before a remote command was sent (while
    /// logging in, refreshing, re-registering or fetching the PIN control
    /// token), so [`Error::may_have_been_received`] reports `false` for a
    /// network failure too.
    #[must_use]
    pub(crate) fn before_command(mut self) -> Self {
        if let Self::Http(e) = &mut self {
            e.before_command = true;
        }
        self
    }

    /// A copy of this error, handed to tasks that waited on the attempt that
    /// produced it (see the client's single-flight login and PIN handling).
    pub(crate) fn duplicate(&self) -> Self {
        match self {
            Self::Authentication(m) => Self::Authentication(m.clone()),
            Self::InvalidPin => Self::InvalidPin,
            Self::PinRequired => Self::PinRequired,
            Self::InvalidDeviceId => Self::InvalidDeviceId,
            Self::DuplicateRequest => Self::DuplicateRequest,
            Self::UnsupportedCommand => Self::UnsupportedCommand,
            Self::UnsupportedVehicle(m) => Self::UnsupportedVehicle(m),
            Self::CarTimeout => Self::CarTimeout,
            Self::ServiceUnavailable => Self::ServiceUnavailable,
            Self::RateLimited => Self::RateLimited,
            Self::NoData => Self::NoData,
            Self::Api { code, message } => Self::Api {
                code: code.clone(),
                message: message.clone(),
            },
            Self::UnexpectedResponse(m) => Self::UnexpectedResponse(m.clone()),
            Self::InvalidTemperature(t) => Self::InvalidTemperature(*t),
            Self::InvalidDuration(d) => Self::InvalidDuration(*d),
            Self::InvalidMonth(m) => Self::InvalidMonth(*m),
            Self::InvalidCoordinates {
                latitude,
                longitude,
            } => Self::InvalidCoordinates {
                latitude: *latitude,
                longitude: *longitude,
            },
            Self::InvalidVehicleId => Self::InvalidVehicleId,
            Self::Http(e) => Self::Http(HttpError {
                inner: Arc::clone(&e.inner),
                before_command: e.before_command,
            }),
        }
    }

    /// Whether a remote command that failed with this error might still
    /// have reached the server, and so might still be carried out by the
    /// car.
    ///
    /// The rule, by variant:
    ///
    /// - `false` — the command was certainly not acted on:
    ///   - refused locally before anything was sent:
    ///     [`Error::InvalidTemperature`], [`Error::InvalidDuration`],
    ///     [`Error::InvalidMonth`], [`Error::InvalidCoordinates`],
    ///     [`Error::InvalidVehicleId`], [`Error::UnsupportedVehicle`],
    ///     [`Error::PinRequired`];
    ///   - [`Error::InvalidPin`]: the PIN step, which comes before the
    ///     command, failed;
    ///   - rejected by the server in a way that proves the command was not
    ///     acted on: [`Error::Authentication`], [`Error::InvalidDeviceId`],
    ///     [`Error::DuplicateRequest`], [`Error::UnsupportedCommand`]. (A
    ///     command whose answer only *looked* like an authentication
    ///     failure, without proving it, is reported as
    ///     [`Error::UnexpectedResponse`] instead, which is `true`.)
    /// - [`Error::Http`]: `false` if the connection could not be made or the
    ///   request could not be built, or if the failure happened before the
    ///   command itself was sent (while logging in, refreshing the session,
    ///   re-registering the device or fetching the PIN control token);
    ///   otherwise `true`.
    /// - `true` for everything else: [`Error::CarTimeout`],
    ///   [`Error::ServiceUnavailable`], [`Error::RateLimited`],
    ///   [`Error::NoData`], [`Error::Api`] and [`Error::UnexpectedResponse`].
    ///   These are conservative: if one of them came from the login or PIN
    ///   step, the command was not sent either, but the variant alone cannot
    ///   show it. Check
    ///   [`Client::command_status`](crate::Client::command_status) or
    ///   [`Client::status`](crate::Client::status) before sending the command
    ///   again.
    #[must_use]
    pub fn may_have_been_received(&self) -> bool {
        match self {
            Self::Http(e) => e.may_have_been_sent(),
            Self::Authentication(_)
            | Self::InvalidPin
            | Self::PinRequired
            | Self::InvalidDeviceId
            | Self::DuplicateRequest
            | Self::UnsupportedCommand
            | Self::UnsupportedVehicle(_)
            | Self::InvalidTemperature(_)
            | Self::InvalidDuration(_)
            | Self::InvalidMonth(_)
            | Self::InvalidCoordinates { .. }
            | Self::InvalidVehicleId => false,
            Self::CarTimeout
            | Self::ServiceUnavailable
            | Self::RateLimited
            | Self::NoData
            | Self::Api { .. }
            | Self::UnexpectedResponse(_) => true,
        }
    }
}

/// A failed HTTP request, from [`Error::Http`].
///
/// Its `Display` says what kind of failure it was; its
/// [`source`](std::error::Error::source) is the underlying transport error,
/// whose own sources carry the details (for example the operating system's
/// "connection refused"). Neither includes the request URL.
#[derive(Debug)]
pub struct HttpError {
    inner: Arc<reqwest::Error>,
    /// The failure happened before a remote command was sent.
    before_command: bool,
}

impl HttpError {
    /// The request timed out, either connecting or waiting for the reply.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        self.inner.is_timeout()
    }

    /// The connection to the server could not be made, so nothing was sent.
    #[must_use]
    pub fn is_connect(&self) -> bool {
        self.inner.is_connect()
    }

    /// Whether the request might have reached the server: anything except
    /// a failure to connect or to build the request, or a failure before
    /// the command itself was sent.
    fn may_have_been_sent(&self) -> bool {
        !(self.before_command || self.inner.is_connect() || self.inner.is_builder())
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = &self.inner;
        f.write_str(if e.is_timeout() {
            "the request timed out"
        } else if e.is_connect() {
            "could not connect to the server"
        } else if e.is_builder() {
            "could not build the request"
        } else if e.is_body() || e.is_decode() {
            "could not read the response"
        } else {
            "the request failed"
        })
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.inner)
    }
}

/// Result type used throughout this crate, defaulting to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_proven_rejections_rule_out_receipt() {
        for e in [
            Error::Authentication("x".into()),
            Error::InvalidDeviceId,
            Error::DuplicateRequest,
            Error::PinRequired,
            Error::InvalidPin,
            Error::InvalidDuration(30),
            Error::InvalidVehicleId,
        ] {
            assert!(!e.may_have_been_received(), "{e:?}");
        }
        for e in [
            Error::CarTimeout,
            Error::ServiceUnavailable,
            Error::RateLimited,
            Error::UnexpectedResponse("x".into()),
            Error::from_res_code("1234", "?"),
        ] {
            assert!(e.may_have_been_received(), "{e:?}");
        }
    }

    #[test]
    fn duplicates_are_identical() {
        for e in [
            Error::Api {
                code: "1234".into(),
                message: "x".into(),
            },
            Error::Authentication("wrong email or password".into()),
            Error::InvalidPin,
            Error::RateLimited,
        ] {
            let copy = e.duplicate();
            assert_eq!(format!("{copy:?}"), format!("{e:?}"));
            assert_eq!(copy.may_have_been_received(), e.may_have_been_received());
        }
    }
}
