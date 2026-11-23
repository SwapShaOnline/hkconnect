//! The MCP tools: reads and remote commands on top of [`hkconnect::Client`].
//!
//! Every tool answers with two text blocks: a one-line summary, then compact
//! JSON with the details. Failures are tool errors (`isError: true`) with a
//! message written for the model, never protocol errors, so the model can
//! read and relay them.
//!
//! Every tool call, reads included, is written to the audit log (tool,
//! arguments, caller, car, outcome; never coordinates or secrets). Text that
//! comes from Hyundai's servers (car names, error messages) has control and
//! bidirectional-override characters removed before it reaches the model.
//!
//! Commands are sent once and never retried here. After the server accepts
//! one, the tool waits (up to [`Settings::command_wait`]) for the car's
//! answer; if it doesn't come in time, the result says so and points to
//! `check_command` instead of inviting a resend.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hkconnect::{
    Brand, Celsius, Client, ClimateOptions, CommandId, CommandStatus, Destination, Session, Vehicle,
};
use jiff::Timestamp;
use jiff::civil::Date;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::audit::{AuditLog, Entry};
use crate::format;
use crate::policy::Policy;
use crate::store::Store;

/// How long the vehicle list is reused before asking the server again.
const VEHICLE_CACHE: Duration = Duration::from_secs(5 * 60);
/// After Bluelink rejects the login, no new login is tried for this long;
/// the pause doubles after each further rejection, up to [`MAX_LOGIN_PAUSE`].
/// Repeated wrong-password logins can get the account locked.
const FIRST_LOGIN_PAUSE: Duration = Duration::from_secs(5 * 60);
/// Longest pause between login attempts after rejections.
const MAX_LOGIN_PAUSE: Duration = Duration::from_secs(6 * 60 * 60);

/// A library error, or a refusal to call the library while logins are
/// paused. Its `Display` includes the whole cause chain (the library keeps
/// network details in `source()`).
#[derive(Debug)]
enum ApiError {
    Api(hkconnect::Error),
    LoginPaused(Duration),
}

impl ApiError {
    /// Whether a command that failed this way might still reach the car.
    fn may_have_been_received(&self) -> bool {
        match self {
            Self::Api(e) => e.may_have_been_received(),
            Self::LoginPaused(_) => false,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(e) => f.write_str(&error_chain(e)),
            Self::LoginPaused(left) => write!(
                f,
                "Bluelink rejected the account's email or password, so hkc-mcp has paused \
                 signing in for another {} min to avoid getting the account locked. The owner \
                 must fix HKC_USERNAME / HKC_PASSWORD on the server and restart hkc-mcp",
                left.as_secs().div_ceil(60).max(1)
            ),
        }
    }
}

/// An error and all its causes: `network error: connection refused`.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    text
}

/// Login back-off state.
#[derive(Debug)]
struct LoginPause {
    until: Option<Instant>,
    next: Duration,
}

impl Default for LoginPause {
    fn default() -> Self {
        Self {
            until: None,
            next: FIRST_LOGIN_PAUSE,
        }
    }
}

/// Placed in the HTTP request's extensions by the OAuth middleware: the
/// client id the bearer token was issued to. Absent over stdio.
#[derive(Debug, Clone)]
pub struct AuthenticatedClient(pub String);

/// Tunables for the tool layer.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The brand, for keying the saved session.
    pub brand: Brand,
    /// The account email, for keying the saved session.
    pub username: String,
    /// The car used when a tool call names none (`HKC_VEHICLE`).
    pub default_vehicle: Option<String>,
    /// Shortest time between two car-waking status refreshes of one car.
    /// Zero disables the limit.
    pub refresh_interval: Duration,
    /// How long a command waits for the car's answer.
    pub command_wait: Duration,
    /// How often the command's status is polled while waiting.
    pub poll_every: Duration,
}

/// Shared state behind every clone of [`CarServer`].
#[derive(Debug)]
struct Backend {
    client: Client,
    settings: Settings,
    store: Option<Store>,
    audit: AuditLog,
    vehicles: Mutex<Option<(Instant, Vec<Vehicle>)>>,
    last_refresh: Mutex<HashMap<String, Instant>>,
    saved_session: Mutex<Option<Session>>,
    login_pause: std::sync::Mutex<LoginPause>,
}

/// The MCP server handler.
#[derive(Debug, Clone)]
pub struct CarServer {
    backend: Arc<Backend>,
    tool_router: ToolRouter<Self>,
}

// ----- parameters -----

/// Selects a car.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VehicleArg {
    /// Which car: its name, VIN or id from `list_vehicles`. Leave out when the
    /// account has one car (or the server has a default car configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
}

/// Arguments for `get_status`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// Wake the car for fresh data instead of returning its last report.
    /// Slower (up to a minute) and drains the car's 12V battery; only set it
    /// when the user needs current data and the cached data is too old. The
    /// server allows at most one refresh per car every few minutes.
    #[serde(default)]
    pub refresh: bool,
}

/// Arguments for `get_trips`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TripsArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// A month as YYYY-MM for that month's totals and the days with trips.
    /// Defaults to the current month (India time) when neither month nor
    /// date is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
    /// A day as YYYY-MM-DD (India time) to list that day's trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// With date: the trip number (1 = earliest that day) to get its GPS
    /// route and driving events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trip: Option<u32>,
}

/// Arguments for `check_command`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckArgs {
    /// The `command_id` returned by an earlier command tool.
    pub command_id: String,
    /// The car the command was sent to: name, VIN or id. Optional when the
    /// account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
}

/// Arguments for `start_climate`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StartArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// Cabin temperature in °C, 14 to 29.5 in steps of 0.5. Default 22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
    /// How long the engine runs, 1 to 10 minutes. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u8>,
    /// Front windscreen defrost. Default off.
    #[serde(default)]
    pub defrost: bool,
    /// Rear window and mirror heating. Default off.
    #[serde(default)]
    pub rear_defrost: bool,
}

/// Arguments for `hazard_lights`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HazardArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// Also sound the horn. Default false (lights only).
    #[serde(default)]
    pub horn: bool,
}

/// Arguments for `set_valet_mode`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ValetArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// true to turn valet mode on, false to turn it off.
    pub on: bool,
}

/// Arguments for `send_destination`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DestinationArgs {
    /// Which car: its name, VIN or id. Optional when the account has one car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// Name of the place, shown on the car's screen.
    pub name: String,
    /// Latitude in decimal degrees (WGS 84), -90 to 90.
    pub latitude: f64,
    /// Longitude in decimal degrees (WGS 84), -180 to 180.
    pub longitude: f64,
    /// Street address shown on the car's screen. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A remote command, validated and ready to send.
#[derive(Debug)]
enum Action {
    Lock,
    Unlock,
    Start(ClimateOptions),
    Stop,
    Hazard { horn: bool },
    Valet { on: bool },
    Navigate(Destination),
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::Unlock => "Unlock",
            Self::Start(_) => "Remote start",
            Self::Stop => "Stop remote start",
            Self::Hazard { horn: true } => "Horn and hazard lights",
            Self::Hazard { horn: false } => "Hazard lights",
            Self::Valet { on: true } => "Valet mode on",
            Self::Valet { on: false } => "Valet mode off",
            Self::Navigate(_) => "Send destination",
        }
    }
}

// ----- results -----

/// Whether `c` could hide or reorder text: a control character or a
/// bidirectional formatting character.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// `text` without control characters (except line breaks) and bidi
/// overrides: server-provided strings can't smuggle hidden text to the
/// model.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || !is_unsafe_char(*c))
        .collect()
}

/// As [`sanitize`], and line breaks become spaces (for one-line values such
/// as car names).
fn sanitize_line(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .filter(|c| !is_unsafe_char(*c))
        .collect()
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::String(s) if s.chars().any(|c| c != '\n' && is_unsafe_char(c)) => {
            *s = sanitize(s);
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize_value),
        Value::Object(map) => map.values_mut().for_each(sanitize_value),
        _ => {}
    }
}

fn blocks(summary: impl Into<String>, data: Option<&Value>) -> Vec<ContentBlock> {
    let mut out = vec![ContentBlock::text(sanitize(&summary.into()))];
    if let Some(data) = data {
        let mut data = data.clone();
        sanitize_value(&mut data);
        out.push(ContentBlock::text(data.to_string()));
    }
    out
}

fn success(summary: impl Into<String>, data: &Value) -> CallToolResult {
    CallToolResult::success(blocks(summary, Some(data)))
}

fn failure(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(blocks(message, None))
}

fn failure_with(summary: impl Into<String>, data: &Value) -> CallToolResult {
    CallToolResult::error(blocks(summary, Some(data)))
}

/// What a read tool learned, for its audit line.
#[derive(Debug, Default)]
struct Trace(std::sync::Mutex<TraceData>);

#[derive(Debug, Default)]
struct TraceData {
    vin: Option<String>,
    command_id: Option<String>,
    outcome: Option<&'static str>,
}

impl Trace {
    fn data(&self) -> std::sync::MutexGuard<'_, TraceData> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn car(&self, car: &Vehicle) {
        self.data().vin = Some(car.vin.clone());
    }

    fn outcome(&self, outcome: &'static str) {
        self.data().outcome = Some(outcome);
    }
}

type ToolResult = Result<CallToolResult, McpError>;

/// The OAuth client behind this call, or `"stdio"`.
fn caller(ctx: &RequestContext<RoleServer>) -> String {
    match ctx.extensions.get::<axum::http::request::Parts>() {
        None => "stdio".into(),
        Some(parts) => parts
            .extensions
            .get::<AuthenticatedClient>()
            .map_or_else(|| "unknown".into(), |c| c.0.clone()),
    }
}

/// Choose the car a call acts on.
///
/// With no selection, an account with several cars is an error rather than
/// a guess. A selection matches, in order: an exact id, a VIN (ignoring
/// case), then a name (ignoring case), which must be unique.
pub fn pick_vehicle(vehicles: &[Vehicle], wanted: Option<&str>) -> Result<Vehicle, String> {
    if vehicles.is_empty() {
        return Err("No vehicles are registered on this Bluelink account.".into());
    }
    let Some(wanted) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        if let [only] = vehicles {
            return Ok(only.clone());
        }
        return Err(format!(
            "This account has {} cars; call again with the `vehicle` argument set to one \
             of these names, VINs or ids:\n{}",
            vehicles.len(),
            list(vehicles.iter())
        ));
    };
    if let Some(v) = vehicles.iter().find(|v| v.id == wanted) {
        return Ok(v.clone());
    }
    if let Some(v) = vehicles.iter().find(|v| v.vin.eq_ignore_ascii_case(wanted)) {
        return Ok(v.clone());
    }
    let wanted_name = wanted.to_lowercase();
    let named: Vec<&Vehicle> = vehicles
        .iter()
        .filter(|v| v.name.trim().to_lowercase() == wanted_name)
        .collect();
    match named.as_slice() {
        [v] => Ok((*v).clone()),
        [] => Err(format!(
            "No car matches {wanted:?}. The account has:\n{}",
            list(vehicles.iter())
        )),
        _ => Err(format!(
            "{} cars are named {wanted:?}; use the VIN or id instead:\n{}",
            named.len(),
            list(named.into_iter())
        )),
    }
}

fn list<'a>(vehicles: impl Iterator<Item = &'a Vehicle>) -> String {
    vehicles
        .map(|v| format!("- {:?} ({}) VIN {} id {}", v.name, v.model, v.vin, v.id))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_month(raw: &str) -> Result<(i16, i8), String> {
    Date::strptime("%Y-%m-%d", format!("{}-01", raw.trim()))
        .map(|d| (d.year(), d.month()))
        .map_err(|_| format!("Invalid month {raw:?}; use YYYY-MM, for example 2026-11."))
}

fn parse_date(raw: &str) -> Result<Date, String> {
    raw.trim()
        .parse::<Date>()
        .map_err(|_| format!("Invalid date {raw:?}; use YYYY-MM-DD, for example 2026-11-17."))
}

fn outcome_name(status: CommandStatus) -> &'static str {
    match status {
        CommandStatus::Succeeded => "succeeded",
        CommandStatus::Failed => "failed",
        CommandStatus::NoResponse => "no_response",
        CommandStatus::Pending => "pending",
        _ => "unknown",
    }
}

const DO_NOT_RESEND: &str = "Do NOT send the command again: it may still be carried out. \
     Use check_command with this command_id (or get_status) to learn what happened.";

impl CarServer {
    /// A server for `client`, offering the tools `policy` allows. `store`
    /// is where the login session is saved after each call (none in tests).
    pub fn new(
        client: Client,
        settings: Settings,
        policy: &Policy,
        store: Option<Store>,
        audit: AuditLog,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        let names: Vec<String> = tool_router.map.keys().map(ToString::to_string).collect();
        for name in names {
            if !policy.allows(&name) {
                tool_router.remove_route(&name);
            }
        }
        Self {
            backend: Arc::new(Backend {
                client,
                settings,
                store,
                audit,
                vehicles: Mutex::new(None),
                last_refresh: Mutex::new(HashMap::new()),
                saved_session: Mutex::new(None),
                login_pause: std::sync::Mutex::new(LoginPause::default()),
            }),
            tool_router,
        }
    }

    /// The tools this server advertises (after policy).
    #[cfg(test)]
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    fn login_pause(&self) -> std::sync::MutexGuard<'_, LoginPause> {
        self.backend
            .login_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every library call goes through here: refused while logins are
    /// paused, and a rejected login (re)starts the pause. The library only
    /// returns `Authentication` after its own re-login attempt failed, so
    /// each one is a login the account rejected.
    async fn api<T>(
        &self,
        call: impl Future<Output = hkconnect::Result<T>>,
    ) -> Result<T, ApiError> {
        if let Some(until) = self.login_pause().until {
            let now = Instant::now();
            if until > now {
                return Err(ApiError::LoginPaused(until - now));
            }
        }
        let result = call.await;
        let mut pause = self.login_pause();
        match &result {
            Ok(_) => *pause = LoginPause::default(),
            Err(hkconnect::Error::Authentication(_)) => {
                let wait = pause.next;
                tracing::error!(
                    minutes = wait.as_secs() / 60,
                    "Bluelink rejected the login; pausing sign-in attempts"
                );
                pause.until = Some(Instant::now() + wait);
                pause.next = (wait * 2).min(MAX_LOGIN_PAUSE);
            }
            Err(_) => {}
        }
        result.map_err(ApiError::Api)
    }

    /// Cars on the account, cached for a few minutes.
    async fn vehicles(&self) -> Result<Vec<Vehicle>, ApiError> {
        let mut cache = self.backend.vehicles.lock().await;
        if let Some((at, list)) = &*cache
            && at.elapsed() < VEHICLE_CACHE
        {
            return Ok(list.clone());
        }
        let mut list = self.api(self.backend.client.vehicles()).await?;
        for v in &mut list {
            v.name = sanitize_line(&v.name);
            v.model = sanitize_line(&v.model);
        }
        *cache = Some((Instant::now(), list.clone()));
        Ok(list)
    }

    /// Resolve the `vehicle` argument (falling back to the configured
    /// default) to a car, or a message for the model.
    async fn car(&self, wanted: Option<&str>) -> Result<Vehicle, String> {
        let vehicles = self
            .vehicles()
            .await
            .map_err(|e| format!("Could not list the account's cars: {e}"))?;
        let wanted = wanted.filter(|w| !w.trim().is_empty()).or(self
            .backend
            .settings
            .default_vehicle
            .as_deref());
        pick_vehicle(&vehicles, wanted)
    }

    /// Save the login session if it changed, so a restart skips the login.
    async fn persist_session(&self) {
        let Some(store) = &self.backend.store else {
            return;
        };
        let Some(session) = self.backend.client.session().await else {
            return;
        };
        let mut saved = self.backend.saved_session.lock().await;
        if saved.as_ref() == Some(&session) {
            return;
        }
        let s = &self.backend.settings;
        match store.save_session(s.brand, &s.username, &session) {
            Ok(()) => *saved = Some(session),
            Err(e) => tracing::warn!(error = %e, "could not save the login session"),
        }
    }

    /// Run a read tool body, save the session, and audit the call.
    async fn read<F>(
        &self,
        tool: &'static str,
        ctx: &RequestContext<RoleServer>,
        arguments: &Value,
        trace: &Trace,
        body: F,
    ) -> ToolResult
    where
        F: Future<Output = Result<CallToolResult, String>>,
    {
        let result = body.await.unwrap_or_else(failure);
        self.persist_session().await;
        let failed = result.is_error == Some(true);
        let error: Option<String> = failed
            .then(|| {
                result
                    .content
                    .first()
                    .and_then(|c| c.as_text())
                    .map(|t| t.text.chars().take(500).collect())
            })
            .flatten();
        let t = std::mem::take(&mut *trace.data());
        self.backend.audit.record(&Entry {
            time: Timestamp::now(),
            tool,
            arguments,
            client: &caller(ctx),
            vin: t.vin.as_deref(),
            command_id: t.command_id.as_deref(),
            outcome: if failed {
                "error"
            } else {
                t.outcome.unwrap_or("ok")
            },
            error: error.as_deref(),
        });
        Ok(result)
    }

    async fn status_body(&self, args: StatusArgs, trace: &Trace) -> Result<CallToolResult, String> {
        let car = self.car(args.vehicle.as_deref()).await?;
        trace.car(&car);
        let client = &self.backend.client;
        let mut skipped = None;
        let live = if args.refresh {
            match self.claim_refresh(&car).await {
                Ok(()) => true,
                Err(wait) => {
                    skipped = Some(wait);
                    false
                }
            }
        } else {
            false
        };
        let result = if live {
            self.api(client.refresh_status(&car)).await
        } else {
            self.api(client.status(&car)).await
        };
        trace.outcome(match (live, skipped) {
            (true, _) => "refreshed",
            (false, Some(_)) => "refresh_skipped",
            (false, None) => "cached",
        });
        let status = result.map_err(|e| {
            if live {
                format!(
                    "Could not refresh {}'s status from the car: {e}. get_status without \
                     refresh returns the last cached report.",
                    car.name
                )
            } else {
                format!("Could not read {}'s status: {e}", car.name)
            }
        })?;
        let now = Timestamp::now();
        let mut data = format::status(&car, &status, live, now);
        let mut summary = format::status_summary(&car, &status, live, now);
        if let Some(wait) = skipped {
            let mins = wait.as_secs().div_ceil(60).max(1);
            let note = format!(
                "Refresh skipped: the car was woken recently, and live refreshes are \
                 limited to once every {} min to protect its 12V battery. This is cached \
                 data; a refresh is allowed again in about {mins} min.",
                self.backend
                    .settings
                    .refresh_interval
                    .as_secs()
                    .div_ceil(60)
            );
            data["refresh_skipped"] = note.clone().into();
            summary = format!("{summary} {note}");
        }
        Ok(success(summary, &data))
    }

    /// Reserve a car-waking refresh for `car`, or say how long until one is
    /// allowed. The slot is taken before the request, so concurrent calls
    /// can't both wake the car, and kept even if the refresh fails (the car
    /// may have been woken anyway).
    async fn claim_refresh(&self, car: &Vehicle) -> Result<(), Duration> {
        let interval = self.backend.settings.refresh_interval;
        let mut last = self.backend.last_refresh.lock().await;
        if let Some(at) = last.get(&car.id) {
            let elapsed = at.elapsed();
            if elapsed < interval {
                return Err(interval.saturating_sub(elapsed));
            }
        }
        last.insert(car.id.clone(), Instant::now());
        Ok(())
    }

    async fn trips_body(&self, args: TripsArgs, trace: &Trace) -> Result<CallToolResult, String> {
        if args.month.is_some() && args.date.is_some() {
            return Err("Give either month or date, not both.".into());
        }
        if args.trip.is_some() && args.date.is_none() {
            return Err("trip needs a date: the trip number is counted within that day.".into());
        }
        let car = self.car(args.vehicle.as_deref()).await?;
        trace.car(&car);
        let client = &self.backend.client;
        let Some(raw_date) = args.date.as_deref() else {
            let (year, month) = if let Some(m) = args.month.as_deref() {
                parse_month(m)?
            } else {
                let today = format::india_today(Timestamp::now());
                (today.year(), today.month())
            };
            let m = self
                .api(client.trips_in_month(&car, year, month))
                .await
                .map_err(|e| format!("Could not read trips for {year:04}-{month:02}: {e}"))?;
            let data = format::trip_month(&car, year, month, &m);
            let count: u32 = m.days.iter().map(|d| d.trips).sum();
            let km = m
                .totals
                .distance_km
                .map(|d| format!(", {d:.0} km"))
                .unwrap_or_default();
            let summary = format!(
                "{}: {count} trips on {} days in {year:04}-{month:02}{km}.",
                car.name,
                m.days.len()
            );
            return Ok(success(summary, &data));
        };
        let date = parse_date(raw_date)?;
        let day = self
            .api(client.trips_on_day(&car, date))
            .await
            .map_err(|e| format!("Could not read trips for {date}: {e}"))?;
        let Some(n) = args.trip else {
            let listing = format::trip_day(&car, &day);
            let summary = format!("{}: {} trips on {date}.", car.name, day.trips.len());
            return Ok(success(summary, &listing));
        };
        let trips = format::sorted_trips(&day);
        let Some(trip) = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| trips.get(i))
        else {
            return Err(format!(
                "{date} has {} trips; trip must be 1 to {0}.",
                trips.len()
            ));
        };
        let detail = self
            .api(client.trip_detail(&car, trip))
            .await
            .map_err(|e| format!("Could not read trip {n} of {date}: {e}"))?;
        let route = format::trip_detail(&car, date, n, &detail);
        let km = detail
            .totals
            .distance_km
            .map_or_else(|| "distance unknown".into(), |d| format!("{d:.1} km"));
        let summary = format!(
            "{}: trip {n} on {date}, {km}, {} route points.",
            car.name,
            detail.route.len()
        );
        Ok(success(summary, &route))
    }

    /// Send `action` to `car`, once.
    async fn send(&self, car: &Vehicle, action: &Action) -> Result<CommandId, ApiError> {
        let client = &self.backend.client;
        match action {
            Action::Lock => self.api(client.lock(car)).await,
            Action::Unlock => self.api(client.unlock(car)).await,
            Action::Start(options) => self.api(client.start_climate(car, options)).await,
            Action::Stop => self.api(client.stop_climate(car)).await,
            Action::Hazard { horn } => self.api(client.hazard_lights(car, *horn)).await,
            Action::Valet { on } => self.api(client.valet_mode(car, *on)).await,
            Action::Navigate(d) => self.api(client.send_destination(car, d)).await,
        }
    }

    /// Send a command once, wait for the car, audit, and describe the
    /// outcome.
    async fn command(
        &self,
        tool: &'static str,
        arguments: Value,
        vehicle: Option<&str>,
        action: Result<Action, hkconnect::Error>,
        client_id: &str,
    ) -> ToolResult {
        let result = self
            .command_inner(tool, &arguments, vehicle, action, client_id)
            .await;
        self.persist_session().await;
        Ok(result)
    }

    async fn command_inner(
        &self,
        tool: &'static str,
        arguments: &Value,
        vehicle: Option<&str>,
        action: Result<Action, hkconnect::Error>,
        client_id: &str,
    ) -> CallToolResult {
        let audit = |vin: Option<&str>, id: Option<&str>, outcome: &str, error: Option<&str>| {
            self.backend.audit.record(&Entry {
                time: Timestamp::now(),
                tool,
                arguments,
                client: client_id,
                vin,
                command_id: id,
                outcome,
                error,
            });
        };
        let action = match action {
            Ok(a) => a,
            Err(e) => {
                let message = format!("Not sent: {e}.");
                audit(None, None, "not_sent", Some(&message));
                return failure(message);
            }
        };
        let label = action.label();
        let car = match self.car(vehicle).await {
            Ok(car) => car,
            Err(message) => {
                audit(None, None, "not_sent", Some(&message));
                return failure(format!("{label} not sent. {message}"));
            }
        };
        let client = &self.backend.client;
        let started = Instant::now();
        let sent = self.send(&car, &action).await;
        let id = match sent {
            Ok(id) => id,
            Err(e) if e.may_have_been_received() => {
                let message = format!(
                    "{label} for {}: {e}. The server may have received the command anyway, \
                     so the car may still act on it. Do NOT send it again; call get_status \
                     in a minute to see whether it took effect.",
                    car.name
                );
                audit(Some(&car.vin), None, "maybe_sent", Some(&e.to_string()));
                return failure(message);
            }
            Err(e) => {
                let message = format!(
                    "{label} for {} was not sent: {e}. Nothing reached the car.",
                    car.name
                );
                audit(Some(&car.vin), None, "not_sent", Some(&e.to_string()));
                return failure(message);
            }
        };
        let s = &self.backend.settings;
        let waited = self
            .api(client.wait_for(&car, &id, s.command_wait, s.poll_every))
            .await;
        let seconds = started.elapsed().as_secs();
        let base = |outcome: &str, message: &str| {
            json!({
                "command_id": id.as_str(),
                "outcome": outcome,
                "seconds": seconds,
                "vehicle": car.name,
                "message": message,
            })
        };
        match waited {
            Ok(status) => {
                let outcome = outcome_name(status);
                audit(Some(&car.vin), Some(id.as_str()), outcome, None);
                let message = describe_outcome(label, status, seconds);
                let data = base(outcome, &message);
                match status {
                    CommandStatus::Failed | CommandStatus::NoResponse => {
                        failure_with(message, &data)
                    }
                    _ => success(message, &data),
                }
            }
            Err(e) => {
                let message = format!(
                    "{label}: the command WAS sent (command_id {id}), but checking its result \
                     failed: {e}. {DO_NOT_RESEND}"
                );
                audit(
                    Some(&car.vin),
                    Some(id.as_str()),
                    "unconfirmed",
                    Some(&e.to_string()),
                );
                success(message.clone(), &base("unknown", &message))
            }
        }
    }
}

/// What a final (or timed-out) command status means, for the model.
fn describe_outcome(label: &str, status: CommandStatus, seconds: u64) -> String {
    match status {
        CommandStatus::Succeeded => format!("{label}: done ({seconds} s)."),
        CommandStatus::Failed => format!(
            "{label}: the car refused the command (for example a door is open, \
             or the engine is already running or off)."
        ),
        CommandStatus::NoResponse => format!(
            "{label}: the car did not respond; it most likely did not act. It may \
             be out of mobile coverage. Check get_status before trying again."
        ),
        _ => format!(
            "{label}: sent, but the car had not answered after {seconds} s. {DO_NOT_RESEND}"
        ),
    }
}

fn args_value(args: &impl Serialize) -> Value {
    serde_json::to_value(args).unwrap_or(Value::Null)
}

#[tool_router]
impl CarServer {
    #[tool(
        description = "List the cars on the owner's Hyundai/Kia (India) account: name, \
                       model, year, VIN and id. Use a car's name, VIN or id as the \
                       `vehicle` argument of other tools when there is more than one.",
        annotations(
            title = "List cars",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_vehicles(&self, ctx: RequestContext<RoleServer>) -> ToolResult {
        let trace = Trace::default();
        self.read("list_vehicles", &ctx, &json!({}), &trace, async {
            let vehicles = self
                .vehicles()
                .await
                .map_err(|e| format!("Could not list the account's cars: {e}"))?;
            let names: Vec<&str> = vehicles.iter().map(|v| v.name.as_str()).collect();
            let summary = format!("{} car(s): {}.", vehicles.len(), names.join(", "));
            let data =
                json!({ "vehicles": vehicles.iter().map(format::vehicle).collect::<Vec<_>>() });
            Ok(success(summary, &data))
        })
        .await
    }

    #[tool(
        description = "Get the car's state: locked or not, doors/boot/bonnet open, engine \
                       and climate, fuel and range, warnings. By default returns the car's \
                       last report, which can be hours old: check `data_age` and say how \
                       old it is. Set refresh=true only if the user needs current data \
                       (wakes the car, slower, uses its 12V battery; limited to once per car \
                       every few minutes). Times are India Standard Time.",
        annotations(
            title = "Car status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        let trace = Trace::default();
        self.read(
            "get_status",
            &ctx,
            &arguments,
            &trace,
            self.status_body(args, &trace),
        )
        .await
    }

    #[tool(
        description = "Where the car was last parked: coordinates, when that position was \
                       recorded (`as_of`, `data_age`, India Standard Time) and a Google Maps \
                       link. It is the last reported parking position, not live tracking.",
        annotations(
            title = "Car location",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_location(
        &self,
        Parameters(args): Parameters<VehicleArg>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let trace = Trace::default();
        self.read("get_location", &ctx, &args_value(&args), &trace, async {
            let car = self.car(args.vehicle.as_deref()).await?;
            trace.car(&car);
            let location = self
                .api(self.backend.client.location(&car))
                .await
                .map_err(|e| format!("Could not read {}'s location: {e}", car.name))?;
            let Some(l) = location else {
                return Ok(success(
                    format!(
                        "{}: the server has no parked position for this car.",
                        car.name
                    ),
                    &json!({ "vehicle": car.name, "location": null }),
                ));
            };
            let now = Timestamp::now();
            let when = l.updated_at.map_or_else(
                || "time unknown".into(),
                |at| format!("as of {} ({})", format::ist_time(at), format::ago(now, at)),
            );
            let summary = format!(
                "{} parked at {:.5}, {:.5}, {when}.",
                car.name, l.latitude, l.longitude
            );
            Ok(success(summary, &format::location(&car, &l, now)))
        })
        .await
    }

    #[tool(
        description = "Details about the car: model, year, transmission, which remote \
                       features it supports, odometer (km), the connectivity subscription \
                       end date and the service intervals.",
        annotations(
            title = "Car details",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_vehicle_info(
        &self,
        Parameters(args): Parameters<VehicleArg>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let trace = Trace::default();
        self.read(
            "get_vehicle_info",
            &ctx,
            &args_value(&args),
            &trace,
            async {
                let car = self.car(args.vehicle.as_deref()).await?;
                trace.car(&car);
                let client = &self.backend.client;
                // Boxed: the two library futures together are large.
                let (caps, maintenance) = Box::pin(async {
                    tokio::try_join!(
                        self.api(client.capabilities(&car)),
                        self.api(client.maintenance(&car))
                    )
                })
                .await
                .map_err(|e| format!("Could not read {}'s details: {e}", car.name))?;
                let odometer = maintenance
                    .odometer_km
                    .map(|k| format!(", odometer {k:.0} km"))
                    .unwrap_or_default();
                let summary = format!(
                    "{}: {} {}{odometer}.",
                    car.name,
                    caps.model.as_deref().unwrap_or(&car.model),
                    caps.model_year.as_deref().unwrap_or("")
                );
                Ok(success(summary, &format::info(&car, &caps, &maintenance)))
            },
        )
        .await
    }

    #[tool(
        description = "Trip history (dates and times in India Standard Time). With no \
                       month or date: this month's totals and the days that had trips. \
                       month=YYYY-MM: that month. date=YYYY-MM-DD: that day's trips, \
                       numbered from 1 (earliest). date plus trip=N: that trip's GPS route \
                       (thinned to at most 400 points) and harsh braking, rapid \
                       acceleration and speeding counts.",
        annotations(
            title = "Trip history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_trips(
        &self,
        Parameters(args): Parameters<TripsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        let trace = Trace::default();
        self.read(
            "get_trips",
            &ctx,
            &arguments,
            &trace,
            self.trips_body(args, &trace),
        )
        .await
    }

    #[tool(
        description = "Look up the result of an earlier command by its command_id (from \
                       lock, unlock, start_climate, ...). Use this when a command's result \
                       was 'pending' or 'unknown' instead of sending the command again. \
                       Outcomes: succeeded, failed (car refused), no_response, pending (car \
                       hasn't answered yet), unknown (the server has no record of it).",
        annotations(
            title = "Check a command",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn check_command(
        &self,
        Parameters(args): Parameters<CheckArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let trace = Trace::default();
        self.read("check_command", &ctx, &args_value(&args), &trace, async {
            let id = args.command_id.trim();
            if id.is_empty() {
                return Err("command_id is empty.".into());
            }
            let car = self.car(args.vehicle.as_deref()).await?;
            trace.car(&car);
            trace.data().command_id = Some(id.to_owned());
            let status = self
                .api(self.backend.client.command_status(&car, &CommandId::new(id)))
                .await
                .map_err(|e| format!("Could not check command {id}: {e}"))?;
            let outcome = outcome_name(status);
            trace.outcome(outcome);
            let meaning = match status {
                CommandStatus::Succeeded => "the car carried it out",
                CommandStatus::Failed => "the car refused it",
                CommandStatus::NoResponse => "the car never answered; it most likely did not act",
                CommandStatus::Pending => "waiting for the car to answer; check again shortly",
                _ => {
                    "the server has no record of this command (it may be too new, too old, \
                     or for another car); use get_status to see the car's state"
                }
            };
            let summary = format!("Command {id} on {}: {outcome} ({meaning}).", car.name);
            let data = json!({ "command_id": id, "vehicle": car.name, "outcome": outcome, "meaning": meaning });
            Ok(success(summary, &data))
        })
        .await
    }

    #[tool(
        description = "Lock the car's doors, then wait up to about 90 s for the car to \
                       confirm. Returns {command_id, outcome, seconds}. If the outcome is \
                       pending or unknown, do not resend: call check_command later.",
        annotations(
            title = "Lock",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn lock(
        &self,
        Parameters(args): Parameters<VehicleArg>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        self.command(
            "lock",
            arguments,
            args.vehicle.as_deref(),
            Ok(Action::Lock),
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Unlock the car's doors. Only do this when the user has clearly asked \
                       to unlock this car right now. Waits up to about 90 s for the car to \
                       confirm and returns {command_id, outcome, seconds}. Never resend an \
                       unlock whose outcome is pending or unknown: call check_command.",
        annotations(
            title = "Unlock",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn unlock(
        &self,
        Parameters(args): Parameters<VehicleArg>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        self.command(
            "unlock",
            arguments,
            args.vehicle.as_deref(),
            Ok(Action::Unlock),
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Remote start: run the engine with climate control for 1-10 minutes \
                       (default 10) at 14-29.5 °C (default 22), optionally with defrost. \
                       Only when the user has clearly asked to start this car. Waits up to \
                       about 90 s for the car to confirm; returns {command_id, outcome, \
                       seconds}. Never resend a start whose outcome is pending or unknown: \
                       call check_command.",
        annotations(
            title = "Remote start with climate",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn start_climate(
        &self,
        Parameters(args): Parameters<StartArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        let action = Celsius::new(args.temperature_c.unwrap_or(22.0)).and_then(|t| {
            Ok(Action::Start(
                ClimateOptions::new(t)
                    .with_duration_minutes(args.minutes.unwrap_or(ClimateOptions::MAX_MINUTES))?
                    .with_defrost(args.defrost)
                    .with_rear_defrost(args.rear_defrost),
            ))
        });
        self.command(
            "start_climate",
            arguments,
            args.vehicle.as_deref(),
            action,
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Stop a remote start (turn the engine and climate off). Waits up to \
                       about 90 s for the car to confirm; returns {command_id, outcome, \
                       seconds}. Needs the owner's PIN to be configured on the server.",
        annotations(
            title = "Stop remote start",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn stop_climate(
        &self,
        Parameters(args): Parameters<VehicleArg>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        self.command(
            "stop_climate",
            arguments,
            args.vehicle.as_deref(),
            Ok(Action::Stop),
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Flash the hazard lights, optionally sounding the horn (horn=true), to \
                       help find the car. Experimental. Needs the owner's PIN on the server. \
                       Returns {command_id, outcome, seconds}.",
        annotations(
            title = "Hazard lights / horn",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn hazard_lights(
        &self,
        Parameters(args): Parameters<HazardArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        self.command(
            "hazard_lights",
            arguments,
            args.vehicle.as_deref(),
            Ok(Action::Hazard { horn: args.horn }),
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Turn valet mode on (on=true) or off (on=false). While on, the car \
                       hides personal data on its screen. Experimental. Needs the owner's \
                       PIN on the server. Returns {command_id, outcome, seconds}.",
        annotations(
            title = "Valet mode",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn set_valet_mode(
        &self,
        Parameters(args): Parameters<ValetArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        self.command(
            "set_valet_mode",
            arguments,
            args.vehicle.as_deref(),
            Ok(Action::Valet { on: args.on }),
            &caller(&ctx),
        )
        .await
    }

    #[tool(
        description = "Send a destination (name, latitude, longitude, optional address) to \
                       the car's navigation system. Look up the coordinates first; do not \
                       guess them. Experimental. Needs the owner's PIN on the server. \
                       Returns {command_id, outcome, seconds}.",
        annotations(
            title = "Send destination",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn send_destination(
        &self,
        Parameters(args): Parameters<DestinationArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let arguments = args_value(&args);
        let action = Destination::new(args.name.trim(), args.latitude, args.longitude)
            .map(|d| Action::Navigate(d.with_address(args.address.clone().unwrap_or_default())));
        self.command(
            "send_destination",
            arguments,
            args.vehicle.as_deref(),
            action,
            &caller(&ctx),
        )
        .await
    }
}

const INSTRUCTIONS: &str = "Reads and controls the owner's Hyundai/Kia car in India through \
the Bluelink / Kia Connect service. Times are India Standard Time. Status and location \
come from the car's last report and can be hours old: always mention `data_age` when it is \
more than a few minutes. Remote commands act on a real car: only send one when the user has \
asked for it, send it once, and if its outcome is pending or unknown use check_command \
rather than sending it again. When the account has several cars, pass the `vehicle` \
argument (name, VIN or id from list_vehicles).";

#[allow(
    clippy::unused_async_trait_impl,
    reason = "the methods are generated by rmcp's tool_handler macro"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CarServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("hkc-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("Hyundai/Kia car (India)"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}
