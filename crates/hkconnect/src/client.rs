//! The high-level async client.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};
use reqwest::Method;
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::de::{flexible_int, flexible_string, object_or_empty};
use crate::envelope::{self, Rejection, Reply};
use crate::model::{
    Capabilities, ClimateOptions, CommandId, CommandRecords, CommandStatus, Destination, Location,
    Maintenance, RawLocation, Vehicle, VehicleList, VehicleStatus, WireMaintenance, WireStatus,
};
use crate::session::{Credentials, Session};
use crate::trips::{
    RawTripDayResponse, RawTripDetailResponse, Trip, TripDay, TripDetail, TripMonth, WireTripMonth,
};
use crate::{Error, Result, auth};

const CCS2_UNSUPPORTED: &str = "this vehicle uses the newer CCS2 protocol: hkconnect can read its location, \
     trips and info, but its status and remote commands are not supported yet";

/// Client for one Bluelink / Kia Connect India account.
///
/// Cheap to share: wrap it in an `Arc` and call it from many tasks. Login and
/// token refresh happen automatically and only once at a time: tasks that
/// were waiting while a login, refresh or PIN request failed receive that
/// same error instead of repeating the request (a call made after the error
/// was returned tries again, except after a rejected PIN; see below).
///
/// # Sessions
///
/// The first request logs in (about 3.5 s) unless a saved [`Session`] was
/// supplied with [`Client::with_session`]. The access token is refreshed when
/// it is within a minute of expiring; if the server rejects the refresh token,
/// the client logs in again (network errors, rate limits and server errors
/// during a refresh are returned instead). If the server rejects the session
/// or the device registration, a read is retried once after logging in or
/// re-registering. Save [`Client::session`] after use to skip the login next
/// time.
///
/// # Commands
///
/// Remote commands return a [`CommandId`] as soon as the server accepts
/// them; the car acts 10–40 s later, or not at all. Use [`Client::wait_for`]
/// (or [`Client::command_status`]) to learn the outcome. Commands marked
/// "Needs the PIN" fail with [`Error::PinRequired`] unless a PIN was given
/// with [`Credentials::with_pin`]. Remote commands have not yet been verified on
/// a real car.
///
/// # A rejected PIN is never resent
///
/// Repeated wrong PINs can block remote control on the account, so once the
/// server rejects the PIN ([`Error::InvalidPin`]) this client remembers it
/// for the rest of its life: every later command that needs the PIN fails
/// with [`Error::InvalidPin`] at once, without contacting the server. The PIN
/// is rejected when the server answers the PIN request without a control
/// token, with an HTTP 4xx status, or with HTTP 401/403 even though the
/// session was obtained by a login or refresh moments before for this very
/// request. (A 401/403 on an older session is taken as an expired session:
/// the client logs in and submits the PIN once more.) To use a corrected PIN,
/// build a new `Client`.
///
/// Commands are never sent twice unless the server's answer proves the first
/// attempt was refused before it reached the car (HTTP 401/403, or result
/// code 7501 or 4002); then the client logs in or re-registers and sends it
/// once more. Any other failure — a gateway timeout, [`Error::RateLimited`],
/// [`Error::ServiceUnavailable`], a network error — is returned as is, and
/// the command *may* still have been carried out: check
/// [`Client::status`] or [`Client::command_status`] before retrying an
/// unlock or a remote start yourself. [`Error::may_have_been_received`]
/// tells the two cases apart.
pub struct Client {
    http: reqwest::Client,
    config: Config,
    credentials: Credentials,
    state: Mutex<State>,
    /// Number of finished login/refresh attempts. Read *before* waiting for
    /// the lock, so a task can tell whether an attempt failed while it
    /// waited.
    session_attempts: AtomicU64,
    /// Number of finished PIN (control token) requests; as above.
    control_attempts: AtomicU64,
    /// The server rejected the PIN; it is never sent again. Only set while
    /// holding the state lock.
    pin_rejected: AtomicBool,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Credentials`' own `Debug` prints the email address; leave it out.
        f.debug_struct("Client")
            .field("config", &self.config)
            .field("username", &"[redacted]")
            .field("has_pin", &self.credentials.has_pin())
            .field("pin_rejected", &self.pin_rejected.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct State {
    session: Option<Session>,
    control: Option<ControlToken>,
    /// The last login/refresh failure, with the attempt number that
    /// produced it (see [`Client::session_attempts`]).
    session_failure: Option<(u64, Error)>,
    /// The last PIN request failure, with its attempt number.
    control_failure: Option<(u64, Error)>,
}

#[derive(Debug)]
struct ControlToken {
    token: secrecy::SecretString,
    expires_at: Timestamp,
}

/// Whether a request may be sent again after an automatic recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Reading data; harmless to repeat, so any session or device rejection
    /// is recovered from and the request resent once.
    Read,
    /// A remote command (unlock, start, ...). Resent once only when the
    /// rejection proves it was not acted on; see [`Rejection::not_executed`].
    Command,
}

/// Shortest gap between polls in [`Client::wait_for`].
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Longest wait honoured by [`Client::wait_for`]: one day.
const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Which token a request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// The normal access token.
    Access,
    /// A short-lived token obtained with the PIN; required by some commands.
    Pin,
}

#[derive(Deserialize)]
struct ControlTokenResponse {
    #[serde(rename = "controlToken", default, deserialize_with = "flexible_string")]
    control_token: Option<String>,
    /// Seconds; accepted as a number or a string.
    #[serde(rename = "expiresTime", default, deserialize_with = "flexible_int")]
    expires_time: Option<i64>,
}

impl Client {
    /// Build a client. Nothing is sent until the first call.
    ///
    /// Fails only if the HTTP client cannot be built.
    pub fn new(config: Config, credentials: Credentials) -> Result<Self> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .gzip(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            // The servers expect the Android app's HTTP library.
            .user_agent("okhttp/3.12.0")
            // Never follow redirects: reqwest does not strip the custom
            // `AuthorizationCCSP` header on a cross-host redirect, and a
            // 307/308 would resend the password or PIN body. No step of the
            // API needs one (sign-in returns its redirect URL in JSON; the
            // authorize page only has to set a cookie, which a 3xx also does).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::http)?;
        Ok(Self {
            http,
            config,
            credentials,
            state: Mutex::new(State::default()),
            session_attempts: AtomicU64::new(0),
            control_attempts: AtomicU64::new(0),
            pin_rejected: AtomicBool::new(false),
        })
    }

    /// Reuse a session saved from an earlier run.
    #[must_use]
    pub fn with_session(mut self, session: Session) -> Self {
        // We own `self`, so no lock is needed (or possible to contend).
        self.state.get_mut().session = Some(session);
        self
    }

    /// The current session, for saving between runs. `None` before the
    /// first login.
    pub async fn session(&self) -> Option<Session> {
        self.state.lock().await.session.clone()
    }

    /// Log in now, even if a session exists, and return the new session.
    ///
    /// Tasks waiting for a session while this runs receive its error if it
    /// fails.
    pub async fn login(&self) -> Result<Session> {
        let mut state = self.state.lock().await;
        let result = auth::login(&self.http, &self.config, &self.credentials).await;
        self.finish_session_attempt(&mut state, result, true)
    }

    // ----- reads -----

    /// Cars on the account. An entry the server sends without a vehicle id
    /// cannot be used, so it is left out.
    pub async fn vehicles(&self) -> Result<Vec<Vehicle>> {
        let list: VehicleList = self.get("/api/v1/spa/vehicles").await?;
        Ok(list.into_vehicles())
    }

    /// The last state the car reported. Fast, and doesn't wake the car, but
    /// can be hours old: check [`VehicleStatus::updated_at`].
    pub async fn status(&self, vehicle: &Vehicle) -> Result<VehicleStatus> {
        require_legacy(vehicle)?;
        let status: WireStatus = self
            .get_object(&vehicle_path(vehicle, "/status/latest")?)
            .await?;
        Ok(status.into_status())
    }

    /// Wake the car and fetch its current state. Slower, and uses the car's
    /// 12V battery: don't call this on a timer.
    pub async fn refresh_status(&self, vehicle: &Vehicle) -> Result<VehicleStatus> {
        require_legacy(vehicle)?;
        let status: WireStatus = self.get_object(&vehicle_path(vehicle, "/status")?).await?;
        Ok(status.into_status())
    }

    /// Where the car was last parked. `None` if the server has no position.
    pub async fn location(&self, vehicle: &Vehicle) -> Result<Option<Location>> {
        let raw: Value = self.get(&vehicle_path(vehicle, "/location/park")?).await?;
        if !raw.is_object() {
            // `null` (no position) or an odd shape: nothing usable.
            return Ok(None);
        }
        let raw: RawLocation = decode(raw)?;
        Ok(raw.into_location())
    }

    /// Odometer and service intervals.
    pub async fn maintenance(&self, vehicle: &Vehicle) -> Result<Maintenance> {
        let raw: WireMaintenance = self
            .get_object(&vehicle_path(vehicle, "/setting/alert/maintenance")?)
            .await?;
        Ok(raw.into_maintenance())
    }

    /// What the car can do remotely, from its factory profile.
    pub async fn capabilities(&self, vehicle: &Vehicle) -> Result<Capabilities> {
        let profile: Value = self.get(&vehicle_path(vehicle, "/profile")?).await?;
        Ok(Capabilities::from_profile(&profile))
    }

    /// Trip totals for a month, and which days had trips. `month` is 1–12.
    pub async fn trips_in_month(
        &self,
        vehicle: &Vehicle,
        year: i16,
        month: i8,
    ) -> Result<TripMonth> {
        if !(1..=12).contains(&month) {
            return Err(Error::InvalidMonth(month));
        }
        let path = vehicle_path(vehicle, "/tripinfo")?;
        let body = json!({ "tripPeriodType": 0, "setTripMonth": format!("{year:04}{month:02}") });
        let raw: WireTripMonth = self.post_object(&path, body).await?;
        Ok(raw.into_month())
    }

    /// The trips taken on a day (dates are in Indian Standard Time).
    pub async fn trips_on_day(&self, vehicle: &Vehicle, date: Date) -> Result<TripDay> {
        let path = vehicle_path(vehicle, "/tripinfo")?;
        let body = json!({ "tripPeriodType": 1, "setTripDay": yyyymmdd(date) });
        let raw: RawTripDayResponse = self.post_object(&path, body).await?;
        Ok(raw.into_day(date))
    }

    /// One trip's GPS route and driving events (harsh braking, rapid
    /// acceleration, speeding).
    ///
    /// `trip` comes from [`Client::trips_on_day`], possibly serialized and
    /// restored in between; the server's identifiers for it are sent back
    /// exactly as they were received.
    pub async fn trip_detail(&self, vehicle: &Vehicle, trip: &Trip) -> Result<TripDetail> {
        let path = vehicle_path(vehicle, "/tripinfo/detail")?;
        let r = &trip.detail_ref;
        let body = json!({
            "tripPeriodType": 1,
            "setTripDay": r.day(),
            "setTripStartTime": r.start,
            "setServiceTID": r.service_tid,
            "tripStartTime": r.start,
            "tripEndTime": r.end,
        });
        let raw: RawTripDetailResponse = self.post_object(&path, body).await?;
        Ok(raw.into_detail())
    }

    // ----- commands -----
    //
    // Each returns as soon as the server accepts the command. The car acts
    // 10–40 s later; use `wait_for` to learn the outcome.

    /// Lock the doors.
    pub async fn lock(&self, vehicle: &Vehicle) -> Result<CommandId> {
        self.door(vehicle, "close").await
    }

    /// Unlock the doors.
    pub async fn unlock(&self, vehicle: &Vehicle) -> Result<CommandId> {
        self.door(vehicle, "open").await
    }

    async fn door(&self, vehicle: &Vehicle, action: &str) -> Result<CommandId> {
        require_legacy(vehicle)?;
        self.command(
            &vehicle_path(vehicle, "/control/door")?,
            Auth::Access,
            |device_id| json!({ "action": action, "deviceId": device_id }),
        )
        .await
    }

    /// Start the engine with climate control. Fails with
    /// [`Error::UnsupportedVehicle`] on CCS2 cars.
    pub async fn start_climate(
        &self,
        vehicle: &Vehicle,
        options: &ClimateOptions,
    ) -> Result<CommandId> {
        require_legacy(vehicle)?;
        let path = vehicle_path(vehicle, "/control/engine")?;
        let body = json!({
            "action": "start",
            "hvacType": 1,
            "options": {
                "defrost": options.defrost(),
                "heating1": u8::from(options.rear_defrost()),
                "igniOnDuration": options.duration_minutes(),
            },
            "tempCode": options.temperature().to_code(),
            "unit": "C",
        });
        self.command(&path, Auth::Access, |_| body.clone()).await
    }

    /// Stop a remote start. Needs the PIN.
    pub async fn stop_climate(&self, vehicle: &Vehicle) -> Result<CommandId> {
        require_legacy(vehicle)?;
        let path = format!(
            "/api/v2/spa/vehicles/{}/control/engine",
            vehicle_id(vehicle)?
        );
        self.command(&path, Auth::Pin, |_| json!({ "action": "stop" }))
            .await
    }

    /// Flash the hazard lights, optionally sounding the horn. Needs the PIN.
    /// Experimental on non-CCS2 cars.
    pub async fn hazard_lights(&self, vehicle: &Vehicle, with_horn: bool) -> Result<CommandId> {
        require_legacy(vehicle)?;
        let kind = if with_horn { "hornlight" } else { "light" };
        let path = format!(
            "/api/v2/spa/vehicles/{}/ccs2/control/{kind}",
            vehicle_id(vehicle)?
        );
        self.command(&path, Auth::Pin, |_| json!({ "command": "on" }))
            .await
    }

    /// Turn valet mode on or off. Needs the PIN. While on, the car hides
    /// personal data on its screen and the app can track its use.
    ///
    /// The server never adds this command to the notification records (seen
    /// on a 2024 Creta), so [`Client::wait_for`] ends with
    /// [`CommandStatus::Unknown`] even when the car carried it out.
    pub async fn valet_mode(&self, vehicle: &Vehicle, on: bool) -> Result<CommandId> {
        require_legacy(vehicle)?;
        let path = format!(
            "/api/v2/spa/vehicles/{}/control/valet",
            vehicle_id(vehicle)?
        );
        let action = if on { "activate" } else { "deactivate" };
        self.command(&path, Auth::Pin, |_| json!({ "action": action }))
            .await
    }

    /// Send a destination to the car's navigation. Needs the PIN.
    pub async fn send_destination(
        &self,
        vehicle: &Vehicle,
        destination: &Destination,
    ) -> Result<CommandId> {
        require_legacy(vehicle)?;
        let path = format!(
            "/api/v2/spa/vehicles/{}/location/routes",
            vehicle_id(vehicle)?
        );
        let poi = json!({
            "phone": "",
            "waypointID": 1,
            "lang": 1,
            "src": "HERE",
            "coord": { "lat": destination.latitude(), "lon": destination.longitude(), "alt": 0, "type": 0 },
            "addr": destination.address(),
            "zip": "",
            "placeid": "",
            "name": destination.name(),
        });
        self.command(
            &path,
            Auth::Pin,
            |device_id| json!({ "deviceID": device_id, "poiInfoList": [poi.clone()] }),
        )
        .await
    }

    /// The current state of a command, checked once.
    ///
    /// A command sent moments ago may not be listed yet and shows as
    /// [`CommandStatus::Unknown`].
    pub async fn command_status(
        &self,
        vehicle: &Vehicle,
        command: &CommandId,
    ) -> Result<CommandStatus> {
        let path = format!("/api/v1/spa/notifications/{}/records", vehicle_id(vehicle)?);
        let records: CommandRecords = self.get(&path).await?;
        Ok(records.status_of(command))
    }

    /// Poll [`Client::command_status`] every `poll_every` until the car
    /// answers or `timeout` passes.
    ///
    /// Returns the final status ([`CommandStatus::Succeeded`],
    /// [`CommandStatus::Failed`] or [`CommandStatus::NoResponse`]) as soon as
    /// it is known. At the deadline it returns the last status seen:
    /// [`CommandStatus::Pending`] if the command is listed but unanswered, or
    /// [`CommandStatus::Unknown`] if it never appeared. Each poll is one cheap
    /// read; it does not wake the car.
    ///
    /// Transient poll failures (network errors, [`Error::ServiceUnavailable`],
    /// [`Error::CarTimeout`], HTTP 5xx) do not end the wait; polling carries
    /// on. If every poll failed, the last error is returned at the deadline.
    /// Other errors, including [`Error::RateLimited`] (polling harder would
    /// only extend the limit), are returned at once.
    ///
    /// `poll_every` is raised to at least one second, and `timeout` is capped
    /// at one day.
    pub async fn wait_for(
        &self,
        vehicle: &Vehicle,
        command: &CommandId,
        timeout: Duration,
        poll_every: Duration,
    ) -> Result<CommandStatus> {
        poll_until(timeout, poll_every, || {
            self.command_status(vehicle, command)
        })
        .await
    }

    // ----- plumbing -----

    async fn post_object<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        let raw: Value = self
            .request(Method::POST, path, Auth::Access, Kind::Read, |_| {
                Some(body.clone())
            })
            .await?
            .body;
        object_or_empty(raw).map_err(|e| Error::UnexpectedResponse(e.to_string()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .request(Method::GET, path, Auth::Access, Kind::Read, |_| None)
            .await?
            .body)
    }

    /// [`Client::get`] for a reply that should be an object; anything else
    /// (`null`, a string) reads as `{}` (see [`object_or_empty`]).
    async fn get_object<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let raw: Value = self.get(path).await?;
        object_or_empty(raw).map_err(|e| Error::UnexpectedResponse(e.to_string()))
    }

    async fn command(
        &self,
        path: &str,
        auth: Auth,
        body: impl Fn(&str) -> Value,
    ) -> Result<CommandId> {
        let reply: Reply<Value> = self
            .request(Method::POST, path, auth, Kind::Command, |device_id| {
                Some(body(device_id))
            })
            .await?;
        reply
            .msg_id
            .map(CommandId::new)
            .ok_or_else(|| Error::UnexpectedResponse("command accepted without a msgId".into()))
    }

    /// Send a request, recovering once from an expired session or a
    /// rejected device registration.
    ///
    /// `kind` decides whether recovery may resend: a [`Kind::Read`] is resent
    /// after any session (7501, 401/403) or device (4002) rejection; a
    /// [`Kind::Command`] only when the rejection proves the server did not
    /// act on it. Failures while getting the PIN control token always count as
    /// "not executed", since the command itself has not been sent yet.
    ///
    /// Every failure before the command itself is sent (login, refresh,
    /// re-registration, PIN) is marked with [`Error::before_command`]. A
    /// command answered with an authentication-looking error that does not
    /// prove it was refused (a 2xx reply with an `"error"` field) is reported
    /// as [`Error::UnexpectedResponse`], since the car may still act on it.
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        auth: Auth,
        kind: Kind,
        body: impl Fn(&str) -> Option<Value>,
    ) -> Result<Reply<T>> {
        if auth == Auth::Pin {
            if !self.credentials.has_pin() {
                return Err(Error::PinRequired);
            }
            if self.pin_rejected.load(Ordering::SeqCst) {
                return Err(Error::InvalidPin);
            }
        }
        let mut recovered = false;
        loop {
            let (session, fresh) = self.ensure_session().await.map_err(Error::before_command)?;
            let mut control_used = None;
            let mut sent = false;
            let attempt = async {
                let token = match auth {
                    Auth::Access => session.access_token.clone(),
                    Auth::Pin => {
                        let token = self.control_token(&session, fresh).await.map_err(|error| {
                            Rejection {
                                error: error.before_command(),
                                not_executed: true,
                            }
                        })?;
                        control_used = Some(token.clone());
                        token
                    }
                };
                sent = true;
                self.send_once::<T>(&method, path, auth, &token, &session, &body)
                    .await
            }
            .await;
            let Rejection {
                error,
                not_executed,
            } = match attempt {
                Ok(reply) => return Ok(reply),
                Err(rejection) => rejection,
            };
            let error = match error {
                Error::Authentication(message)
                    if kind == Kind::Command && sent && !not_executed =>
                {
                    Error::UnexpectedResponse(format!(
                        "the command was answered with an error: {message}"
                    ))
                }
                other => other,
            };

            let may_recover = !recovered
                && match kind {
                    Kind::Read => true,
                    Kind::Command => not_executed,
                };
            match error {
                Error::Authentication(reason) if may_recover => {
                    tracing::debug!(%reason, "session rejected; logging in again");
                    let mut state = self.state.lock().await;
                    // Compare-and-clear: another task may already have
                    // replaced the session this request used. Wiping the
                    // fresh one would force a needless second login.
                    if state
                        .session
                        .as_ref()
                        .is_some_and(|s| s.access_token == session.access_token)
                    {
                        state.session = None;
                        state.control = None;
                    } else if let Some(used) = &control_used
                        && state
                            .control
                            .as_ref()
                            .is_some_and(|c| c.token.expose_secret() == used)
                    {
                        state.control = None;
                    }
                }
                Error::InvalidDeviceId if may_recover => {
                    tracing::debug!("device id rejected; re-registering");
                    let device_id = auth::register_device(&self.http, &self.config)
                        .await
                        .map_err(Error::before_command)?;
                    let mut state = self.state.lock().await;
                    // Only replace the id this request used; if another task
                    // re-registered meanwhile, keep its id.
                    if let Some(s) = state.session.as_mut()
                        && s.device_id == session.device_id
                    {
                        s.device_id = device_id;
                        state.control = None;
                    }
                }
                other => return Err(other),
            }
            recovered = true;
        }
    }

    /// One attempt at a request with the given token.
    async fn send_once<T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
        auth: Auth,
        token: &str,
        session: &Session,
        body: &impl Fn(&str) -> Option<Value>,
    ) -> Result<Reply<T>, Rejection> {
        let mut req = self
            .http
            .request(method.clone(), self.config.url(path))
            .header("Authorization", token)
            .header("ccsp-service-id", self.config.service_id)
            .header("ccsp-application-id", self.config.app_id)
            .header("ccsp-device-id", &session.device_id);
        if auth == Auth::Pin {
            req = req.header("AuthorizationCCSP", token);
        }
        if let Some(b) = body(&session.device_id) {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        tracing::debug!(%method, path, %status, "api call");
        envelope::parse_classified(status, &bytes)
    }

    /// A usable session, and whether it was obtained (by a login or a
    /// refresh) during this call.
    ///
    /// Single-flight: one task logs in or refreshes while the others wait for
    /// the lock. If that attempt fails, the tasks that were waiting get the
    /// same error rather than trying again themselves.
    async fn ensure_session(&self) -> Result<(Session, bool)> {
        let seen = self.session_attempts.load(Ordering::SeqCst);
        let mut state = self.state.lock().await;
        if let Some(session) = &state.session
            && session.is_fresh()
        {
            return Ok((session.clone(), false));
        }
        if let Some((attempt, error)) = &state.session_failure
            && *attempt > seen
        {
            tracing::debug!(%error, "reusing the error of the login attempt this task waited for");
            return Err(error.duplicate());
        }
        let mut refreshed = None;
        if let Some(session) = &state.session {
            match auth::refresh(&self.http, &self.config, session).await {
                // Only a refused refresh token justifies a full login; a
                // network error, rate limit or outage would just hit the
                // login endpoints too.
                Err(e) if refresh_was_rejected(&e) => {
                    tracing::debug!(error = %e, "refresh rejected; logging in");
                }
                other => refreshed = Some(other),
            }
        }
        let logged_in = refreshed.is_none();
        let result = match refreshed {
            Some(result) => result,
            None => auth::login(&self.http, &self.config, &self.credentials).await,
        };
        self.finish_session_attempt(&mut state, result, logged_in)
            .map(|session| (session, true))
    }

    /// Record the outcome of a login or refresh under the lock.
    fn finish_session_attempt(
        &self,
        state: &mut State,
        result: Result<Session>,
        logged_in: bool,
    ) -> Result<Session> {
        let attempt = self.session_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        match result {
            Ok(session) => {
                state.session = Some(session.clone());
                state.session_failure = None;
                if logged_in {
                    state.control = None;
                }
                Ok(session)
            }
            Err(e) => {
                state.session_failure = Some((attempt, e.duplicate()));
                Err(e)
            }
        }
    }

    /// The PIN control token, from the cache or the server.
    ///
    /// `fresh` says the session was obtained during this request, so an
    /// HTTP 401/403 from the PIN endpoint cannot be an expired session and is
    /// taken as a rejected PIN. A rejected PIN is remembered and never sent
    /// again (see "A rejected PIN is never resent" on [`Client`]); other
    /// failures are handed to the tasks that waited for this attempt.
    async fn control_token(&self, session: &Session, fresh: bool) -> Result<String> {
        let pin = self.credentials.pin().ok_or(Error::PinRequired)?;
        let seen = self.control_attempts.load(Ordering::SeqCst);
        let mut state = self.state.lock().await;
        if self.pin_rejected.load(Ordering::SeqCst) {
            return Err(Error::InvalidPin);
        }
        if let Some(c) = &state.control
            && c.expires_at > Timestamp::now() + SignedDuration::from_secs(30)
        {
            return Ok(c.token.expose_secret().to_owned());
        }
        if let Some((attempt, error)) = &state.control_failure
            && *attempt > seen
        {
            return Err(error.duplicate());
        }

        let result = self.request_control_token(session, pin).await;
        let attempt = self.control_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        match result {
            Ok((token, expires_in)) => {
                let token = format!("Bearer {token}");
                state.control = Some(ControlToken {
                    token: token.clone().into(),
                    expires_at: auth::expiry_from_now(expires_in),
                });
                state.control_failure = None;
                Ok(token)
            }
            Err(e) => {
                let e = match e {
                    Error::Authentication(reason) if fresh => {
                        tracing::debug!(%reason, "PIN request refused right after login");
                        Error::InvalidPin
                    }
                    Error::Api { code, message } if auth::is_http_4xx(&code) => {
                        tracing::debug!(%code, %message, "PIN request refused");
                        Error::InvalidPin
                    }
                    other => other,
                };
                if matches!(e, Error::InvalidPin) {
                    tracing::warn!(
                        "the PIN was rejected; it will not be sent again by this client"
                    );
                    self.pin_rejected.store(true, Ordering::SeqCst);
                }
                state.control_failure = Some((attempt, e.duplicate()));
                Err(e)
            }
        }
    }

    /// `PUT /user/pin`: the control token and its lifetime in seconds.
    async fn request_control_token(&self, session: &Session, pin: &str) -> Result<(String, i64)> {
        let resp = self
            .http
            .put(self.config.url("/api/v1/user/pin?token="))
            .header("Authorization", &session.access_token)
            .json(&json!({ "deviceId": session.device_id, "pin": pin }))
            .send()
            .await
            .map_err(Error::http)?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(Error::http)?;
        tracing::debug!(method = "PUT", path = "/api/v1/user/pin", %status, "api call");
        let parsed: Reply<ControlTokenResponse> = envelope::parse(status, &bytes)?;
        match (parsed.body.control_token, parsed.body.expires_time) {
            (Some(token), Some(expires_in)) if !token.is_empty() => Ok((token, expires_in)),
            _ => Err(Error::InvalidPin),
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::UnexpectedResponse(e.to_string()))
}

/// Whether a refresh failure means the refresh token itself was refused.
fn refresh_was_rejected(e: &Error) -> bool {
    match e {
        Error::Authentication(_) => true,
        Error::Api { code, .. } => auth::is_http_4xx(code),
        _ => false,
    }
}

/// A poll failure worth polling through: the next attempt may well succeed.
fn is_transient(e: &Error) -> bool {
    match e {
        Error::Http(_) | Error::ServiceUnavailable | Error::CarTimeout => true,
        Error::Api { code, .. } => code.len() == 3 && code.starts_with('5'),
        _ => false,
    }
}

/// The polling loop behind [`Client::wait_for`], separate so it can be tested
/// with a paused clock.
async fn poll_until<F, Fut>(
    timeout: Duration,
    poll_every: Duration,
    mut poll: F,
) -> Result<CommandStatus>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<CommandStatus>>,
{
    let poll_every = poll_every.max(MIN_POLL_INTERVAL);
    let start = tokio::time::Instant::now();
    // Capped, so this cannot overflow in practice; if it somehow did, poll once.
    let deadline = start.checked_add(timeout.min(MAX_WAIT)).unwrap_or(start);
    let mut last_status = None;
    let mut last_error = None;
    loop {
        match poll().await {
            Ok(status) if status.is_final() => return Ok(status),
            Ok(status) => last_status = Some(status),
            Err(e) if is_transient(&e) => {
                tracing::debug!(error = %e, "command status poll failed; retrying");
                last_error = Some(e);
            }
            Err(e) => return Err(e),
        }
        let next = tokio::time::Instant::now().checked_add(poll_every);
        if next.is_none_or(|next| next > deadline) {
            return match (last_status, last_error) {
                (Some(status), _) => Ok(status),
                (None, Some(e)) => Err(e),
                (None, None) => unreachable!("every poll sets a status or an error"),
            };
        }
        tokio::time::sleep(poll_every).await;
    }
}

fn yyyymmdd(date: Date) -> String {
    date.strftime("%Y%m%d").to_string()
}

/// The vehicle's id, checked to be safe as one URL path segment.
///
/// [`Vehicle`] is `Deserialize` with public fields, so its id may come from
/// anywhere. An id such as `../../user/pin?` would otherwise make
/// `Url::join` reach a different endpoint, so anything but ASCII letters,
/// digits, `-`, `_` and `.` is refused, as are `.` and anything containing
/// `..`.
fn vehicle_id(vehicle: &Vehicle) -> Result<&str> {
    let id = vehicle.id.as_str();
    let usable = !id.is_empty()
        && id != "."
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if usable {
        Ok(id)
    } else {
        Err(Error::InvalidVehicleId)
    }
}

fn vehicle_path(vehicle: &Vehicle, suffix: &str) -> Result<String> {
    Ok(format!(
        "/api/v1/spa/vehicles/{}{suffix}",
        vehicle_id(vehicle)?
    ))
}

fn require_legacy(vehicle: &Vehicle) -> Result<()> {
    if vehicle.ccs2 {
        Err(Error::UnsupportedVehicle(CCS2_UNSUPPORTED))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn polling_survives_transient_errors() {
        let calls = AtomicU32::new(0);
        let status = poll_until(Duration::from_secs(60), Duration::from_secs(2), || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(Error::ServiceUnavailable),
                1 => Ok(CommandStatus::Pending),
                2 => Err(Error::CarTimeout),
                _ => Ok(CommandStatus::Succeeded),
            }
        })
        .await
        .unwrap();
        assert_eq!(status, CommandStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn polling_returns_last_status_or_last_error_at_the_deadline() {
        let start = tokio::time::Instant::now();
        let calls = AtomicU32::new(0);
        let status = poll_until(Duration::from_secs(10), Duration::from_secs(1), || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(CommandStatus::Pending)
            } else {
                Err(Error::ServiceUnavailable)
            }
        })
        .await
        .unwrap();
        assert_eq!(status, CommandStatus::Pending);
        assert!(start.elapsed() <= Duration::from_secs(10));

        let err = poll_until(Duration::from_secs(5), Duration::from_secs(1), || async {
            Err::<CommandStatus, _>(Error::ServiceUnavailable)
        })
        .await
        .unwrap_err();
        assert!(matches!(err, Error::ServiceUnavailable));
    }

    #[tokio::test(start_paused = true)]
    async fn polling_stops_on_non_transient_errors() {
        let calls = AtomicU32::new(0);
        let err = poll_until(Duration::from_secs(60), Duration::from_secs(1), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<CommandStatus, _>(Error::RateLimited)
        })
        .await
        .unwrap_err();
        assert!(matches!(err, Error::RateLimited));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_interval_and_huge_timeout_are_tamed() {
        let start = tokio::time::Instant::now();
        let calls = AtomicU32::new(0);
        poll_until(Duration::MAX, Duration::ZERO, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(CommandStatus::Unknown)
        })
        .await
        .unwrap();
        // One poll per second for the one-day cap, not a busy loop or a panic.
        let polls = calls.load(Ordering::SeqCst);
        assert!((86_000..=86_401).contains(&polls), "{polls}");
        assert!(start.elapsed() <= MAX_WAIT);
    }
}
