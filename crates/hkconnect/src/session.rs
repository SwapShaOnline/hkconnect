use std::fmt;

use jiff::{SignedDuration, Timestamp};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

/// Account login details.
///
/// ```
/// let credentials = hkconnect::Credentials::new("you@example.com", "password").with_pin("1234");
/// assert!(credentials.has_pin());
/// ```
///
/// The password and PIN are held as secrets: they are never printed by
/// `Debug` and are wiped from memory when dropped.
#[derive(Clone)]
pub struct Credentials {
    username: String,
    password: SecretString,
    pin: Option<SecretString>,
}

impl Credentials {
    /// Login details: the account's email and password.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: SecretString::from(password.into()),
            pin: None,
        }
    }

    /// Add the 4-digit PIN set in the app. It is needed for commands that
    /// use a control token (stop, hazard lights, valet mode, send
    /// destination); without it they fail with
    /// [`Error::PinRequired`](crate::Error::PinRequired).
    #[must_use]
    pub fn with_pin(mut self, pin: impl Into<String>) -> Self {
        self.pin = Some(SecretString::from(pin.into()));
        self
    }

    /// The account's login email.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Whether a PIN was given with [`Credentials::with_pin`].
    #[must_use]
    pub fn has_pin(&self) -> bool {
        self.pin.is_some()
    }

    pub(crate) fn password(&self) -> &str {
        self.password.expose_secret()
    }

    pub(crate) fn pin(&self) -> Option<&str> {
        self.pin.as_ref().map(SecretString::expose_secret)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("pin", &self.pin.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// A logged-in session. Persist it (securely) to skip the ~3.5 s login on
/// the next run; the client refreshes it automatically.
///
/// Get one from [`Client::session`](crate::Client::session) or
/// [`Client::login`](crate::Client::login), and restore it with
/// [`Client::with_session`](crate::Client::with_session). It serializes the
/// access and refresh tokens in plain text, so store it with restricted
/// permissions. Its `Debug` output redacts the tokens.
///
/// The serialized form carries a format [`version`](Session::version). A
/// session saved by a newer release may have a version this release does
/// not know ([`Session::is_supported`] is `false`); discard it and log in
/// again rather than using it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct Session {
    #[serde(default = "Session::first_version")]
    pub(crate) version: u32,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at: Timestamp,
    pub(crate) device_id: String,
}

impl Session {
    /// The serialized format version written by this release.
    pub const FORMAT_VERSION: u32 = 1;

    /// Sessions saved before the version field existed are version 1.
    fn first_version() -> u32 {
        1
    }

    pub(crate) fn new(
        access_token: String,
        refresh_token: String,
        expires_at: Timestamp,
        device_id: String,
    ) -> Self {
        Self {
            version: Self::FORMAT_VERSION,
            access_token,
            refresh_token,
            expires_at,
            device_id,
        }
    }

    /// Usable for at least another minute.
    pub(crate) fn is_fresh(&self) -> bool {
        Timestamp::now() + SignedDuration::from_secs(60) < self.expires_at
    }

    /// When the access token expires. The client refreshes the session
    /// shortly before this, using the refresh token.
    #[must_use]
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// The format version this session was saved with.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether this release understands the session's format version
    /// ([`Session::FORMAT_VERSION`]).
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.version == Self::FORMAT_VERSION
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("version", &self.version)
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("device_id", &self.device_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new(
            "Bearer secret-a".into(),
            "secret-r".into(),
            Timestamp::now(),
            "dev".into(),
        )
    }

    #[test]
    fn debug_never_prints_tokens() {
        let out = format!("{:?}", session());
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn debug_never_prints_password_or_pin() {
        let out = format!("{:?}", Credentials::new("me", "hunter2").with_pin("4321"));
        assert!(!out.contains("hunter2") && !out.contains("4321"), "{out}");
    }

    #[test]
    fn version_defaults_to_1_and_round_trips() {
        let s = session();
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(serde_json::from_value::<Session>(json.clone()).unwrap(), s);

        let mut old = json;
        old.as_object_mut().unwrap().remove("version");
        let old: Session = serde_json::from_value(old).unwrap();
        assert!(old.is_supported());

        let mut future = serde_json::to_value(&s).unwrap();
        future["version"] = 2.into();
        let future: Session = serde_json::from_value(future).unwrap();
        assert!(!future.is_supported());
    }
}
