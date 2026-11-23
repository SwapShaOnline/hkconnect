//! Typed views of the API's responses.
//!
//! The server's JSON is read through private "wire" structs that tolerate
//! missing, null and oddly typed fields, and skip list entries that cannot
//! be used, so a new or different model never breaks parsing. The public
//! types derive symmetric serde with their own `snake_case` field names: what
//! they serialize (e.g. for `--json` output, or to persist) deserializes
//! back to the same value.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::de::{
    self, flexible_bool, flexible_bool_or_false, flexible_f64, flexible_int, flexible_string,
    ist_timestamp, lenient, lenient_vec,
};
use crate::temperature::{self, Celsius};
use crate::{Error, Result};

/// A car on the account, as returned by [`Client::vehicles`](crate::Client::vehicles).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Vehicle {
    /// The server's id for this car, used in every vehicle request.
    pub id: String,
    /// Vehicle identification number; empty if not reported.
    pub vin: String,
    /// The nickname set in the app, or the model name if there is none.
    pub name: String,
    /// Model name, e.g. `"Creta"`; empty if not reported.
    pub model: String,
    /// Model year as sent by the server; `None` if not reported.
    pub year: Option<String>,
    /// Engine type.
    pub powertrain: Powertrain,
    /// Newer cars speak the "CCS2" protocol, which uses different endpoints.
    /// Status reads and remote commands return
    /// [`Error::UnsupportedVehicle`](crate::Error::UnsupportedVehicle) for
    /// such cars.
    pub ccs2: bool,
}

/// A car's engine type, from the server's `type` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Powertrain {
    /// Internal combustion (`"GN"`): petrol or diesel.
    Petrol,
    /// Battery electric (`"EV"`).
    Electric,
    /// Plug-in hybrid (`"PHEV"` or `"PE"`).
    PlugInHybrid,
    /// Hybrid (`"HV"`).
    Hybrid,
    /// Missing or unrecognised code.
    #[default]
    Unknown,
}

impl Powertrain {
    fn from_code(code: Option<&str>) -> Self {
        match code {
            // "GN" = general (internal combustion: petrol or diesel).
            Some("GN") => Self::Petrol,
            Some("EV") => Self::Electric,
            Some("PHEV" | "PE") => Self::PlugInHybrid,
            Some("HV") => Self::Hybrid,
            _ => Self::Unknown,
        }
    }
}

/// A snapshot of the car's state, from [`Client::status`](crate::Client::status)
/// (cached) or [`Client::refresh_status`](crate::Client::refresh_status) (live).
///
/// Every field is `None` when the car did not report it; which fields are
/// present varies by model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VehicleStatus {
    /// When the car reported this state. Cached status can be hours old.
    pub updated_at: Option<Timestamp>,
    /// Whether the engine is running.
    pub engine_on: Option<bool>,
    /// Whether climate control is on.
    pub climate_on: Option<bool>,
    /// Climate set-point. `None` unless climate is on, since the car reports
    /// a meaningless 14 °C while it is off.
    pub climate_temperature: Option<Celsius>,
    /// Whether the front defroster is on.
    pub defrost_on: Option<bool>,
    /// Whether the doors are locked.
    pub locked: Option<bool>,
    /// Which doors are open.
    pub doors_open: Option<Doors>,
    /// Whether the trunk (boot) is open.
    pub trunk_open: Option<bool>,
    /// Whether the hood (bonnet) is open.
    pub hood_open: Option<bool>,
    /// Whether the sunroof is open.
    pub sunroof_open: Option<bool>,
    /// Fuel level, 0–100 %.
    pub fuel_percent: Option<f64>,
    /// Estimated distance to empty.
    pub range: Option<Distance>,
    /// Whether the low-fuel warning light is on.
    pub low_fuel_warning: Option<bool>,
    /// Whether the overall tyre-pressure warning lamp is on.
    pub tyre_pressure_warning: Option<bool>,
    /// Whether the washer fluid is low.
    pub washer_fluid_low: Option<bool>,
    /// Whether the brake fluid warning is on.
    pub brake_fluid_warning: Option<bool>,
    /// Whether the smart key's battery is low.
    pub smart_key_battery_low: Option<bool>,
    /// Whether the hazard lights are on.
    pub hazard_lights_on: Option<bool>,
    /// The 12V starter battery.
    pub battery_12v: Option<Battery12V>,
}

impl VehicleStatus {
    /// True if any door, the trunk, or the hood is open.
    #[must_use]
    pub fn anything_open(&self) -> bool {
        self.doors_open.as_ref().is_some_and(Doors::any)
            || self.trunk_open == Some(true)
            || self.hood_open == Some(true)
    }
}

/// Which doors are open. A door the car did not report counts as closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
#[expect(clippy::struct_excessive_bools, reason = "one flag per door")]
pub struct Doors {
    /// Front-left door.
    pub front_left: bool,
    /// Front-right door.
    pub front_right: bool,
    /// Rear-left door.
    pub rear_left: bool,
    /// Rear-right door.
    pub rear_right: bool,
}

impl Doors {
    /// True if any door is open.
    #[must_use]
    pub fn any(&self) -> bool {
        self.front_left || self.front_right || self.rear_left || self.rear_right
    }
}

/// A distance with its unit, as reported by the car.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Distance {
    /// The distance in [`unit`](Self::unit); `None` if not reported.
    pub value: Option<f64>,
    /// The unit of `value`.
    pub unit: DistanceUnit,
}

/// Unit of a [`Distance`]. The server sends a numeric code: 2 or 3 means
/// miles, anything else kilometres.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DistanceUnit {
    /// Kilometres (also used when the unit is missing or unrecognised).
    #[default]
    Km,
    /// Miles.
    Miles,
}

impl DistanceUnit {
    fn from_code(code: Option<i64>) -> Self {
        match code {
            Some(2 | 3) => Self::Miles,
            _ => Self::Km,
        }
    }
}

/// The 12V starter battery. Frequent [`Client::refresh_status`](crate::Client::refresh_status)
/// calls drain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Battery12V {
    /// State of charge in percent, when the car reports it.
    pub percent: Option<u8>,
    /// Whether the car reports a 12V battery warning.
    pub warning: Option<bool>,
}

/// Where the car was last parked, from [`Client::location`](crate::Client::location).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Location {
    /// Latitude in degrees (WGS 84).
    pub latitude: f64,
    /// Longitude in degrees (WGS 84).
    pub longitude: f64,
    /// Altitude in metres, if reported.
    pub altitude_m: Option<f64>,
    /// Speed when the position was recorded, as sent by the server (normally
    /// 0 while parked; the server's unit code is ignored).
    pub speed: Option<f64>,
    /// When the position was recorded.
    pub updated_at: Option<Timestamp>,
}

/// Odometer and service intervals, from
/// [`Client::maintenance`](crate::Client::maintenance).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Maintenance {
    /// Total distance driven, in km.
    pub odometer_km: Option<f64>,
    /// Service items and their intervals.
    pub items: Vec<MaintenanceItem>,
}

/// One service item, such as engine oil or the air filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MaintenanceItem {
    /// Item name as sent by the server, e.g. `"Engine oil"`.
    pub name: String,
    /// Whether reminders for this item are switched on.
    pub enabled: bool,
    /// How often the item is due.
    pub interval: ServiceInterval,
}

/// How often a service item is due: by distance, time, or both, whichever
/// comes first. Each is `None` if not reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ServiceInterval {
    /// Distance between services, in km.
    pub km: Option<u32>,
    /// Time between services, in months. `0` means no time-based interval.
    pub months: Option<u32>,
}

/// What the car is equipped to do remotely, from its factory profile
/// ([`Client::capabilities`](crate::Client::capabilities)).
///
/// Text fields are `None` and flags `false` when the profile omits them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a capability list is a set of flags"
)]
pub struct Capabilities {
    /// Model name from the factory profile.
    pub model: Option<String>,
    /// Model year from the factory profile.
    pub model_year: Option<String>,
    /// Transmission type, as sent by the server.
    pub transmission: Option<String>,
    /// Remote control (including remote start) is fitted.
    pub remote_start: bool,
    /// Remote horn and hazard lights are available.
    pub horn_and_lights: bool,
    /// Remote hazard lights without the horn are available.
    pub lights_only: bool,
    /// Heated rear window.
    pub rear_defrost: bool,
    /// Heated steering wheel.
    pub steering_wheel_heat: bool,
    /// Ventilated seats.
    pub seat_ventilation: bool,
    /// Telematics subscription end date, e.g. `"20290529"`.
    pub connectivity_ends: Option<String>,
}

impl Capabilities {
    pub(crate) fn from_profile(profile: &Value) -> Self {
        let info = &profile["vinInfo"][0];
        let flag =
            |section: &str, key: &str| info[section].get(key).and_then(de::truthy).unwrap_or(false);
        let text = |section: &str, key: &str| match &info[section][key] {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Self {
            model: text("basic", "modelName"),
            model_year: text("basic", "modelYear"),
            transmission: text("basic", "transmission"),
            remote_start: flag("option", "remoteControl"),
            horn_and_lights: flag("option", "hornLightAvailable"),
            lights_only: flag("option", "lightOnlyAvailable"),
            rear_defrost: flag("option", "heatingRearWindow"),
            steering_wheel_heat: flag("option", "heatingSteeringWheel"),
            seat_ventilation: flag("option", "seatVentilation"),
            connectivity_ends: text("device", "simEndDate").or_else(|| text("basic", "simEndDate")),
        }
    }
}

/// Identifies a command sent to the car, to check its result later with
/// [`Client::command_status`](crate::Client::command_status) or
/// [`Client::wait_for`](crate::Client::wait_for).
///
/// This is the server's `msgId` for the accepted request. It serializes as
/// a plain string, and [`CommandId::new`] rebuilds one from that string (for
/// example, an id saved or shown to the user earlier).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    /// A command id from its string form.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as sent by the server.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What happened to a command, from the server's command records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandStatus {
    /// Waiting for the car to answer.
    Pending,
    /// The car carried it out.
    Succeeded,
    /// The car refused it (e.g. a door is open, or the engine is running).
    Failed,
    /// The car never answered; it most likely did not act.
    NoResponse,
    /// The server has no record of this command, or reported a result this
    /// crate does not recognise.
    Unknown,
}

impl CommandStatus {
    /// True once the car has answered: [`Succeeded`](Self::Succeeded),
    /// [`Failed`](Self::Failed) or [`NoResponse`](Self::NoResponse).
    ///
    /// [`Pending`](Self::Pending) and [`Unknown`](Self::Unknown) are not
    /// final: a just-sent command may not be listed yet.
    #[must_use]
    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::NoResponse)
    }
}

/// Options for remote start with climate, passed to
/// [`Client::start_climate`](crate::Client::start_climate).
///
/// Built from a temperature with [`ClimateOptions::new`] (10 minutes, both
/// defrosters off), then adjusted with the `with_*` methods. The duration is
/// checked when it is set, so a `ClimateOptions` is always valid.
///
/// ```
/// use hkconnect::{Celsius, ClimateOptions};
///
/// let options = ClimateOptions::new(Celsius::new(21.5)?)
///     .with_duration_minutes(5)?
///     .with_defrost(true);
/// assert_eq!(options.duration_minutes(), 5);
/// # Ok::<(), hkconnect::Error>(())
/// ```
///
/// The default is 22 °C for 10 minutes with both defrosters off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateOptions {
    temperature: Celsius,
    duration_minutes: u8,
    defrost: bool,
    rear_defrost: bool,
}

impl ClimateOptions {
    /// The longest remote start the API accepts, in minutes.
    pub const MAX_MINUTES: u8 = 10;

    /// Run for 10 minutes at `temperature`, with both defrosters off.
    #[must_use]
    pub fn new(temperature: Celsius) -> Self {
        Self {
            temperature,
            duration_minutes: Self::MAX_MINUTES,
            defrost: false,
            rear_defrost: false,
        }
    }

    /// How long the engine runs. Fails with
    /// [`Error::InvalidDuration`](crate::Error::InvalidDuration) unless it
    /// is 1–10 minutes.
    pub fn with_duration_minutes(mut self, minutes: u8) -> Result<Self> {
        if !(1..=Self::MAX_MINUTES).contains(&minutes) {
            return Err(Error::InvalidDuration(minutes));
        }
        self.duration_minutes = minutes;
        Ok(self)
    }

    /// Turn the front windscreen defrost on or off.
    #[must_use]
    pub fn with_defrost(mut self, on: bool) -> Self {
        self.defrost = on;
        self
    }

    /// Turn rear window and mirror heating on or off.
    #[must_use]
    pub fn with_rear_defrost(mut self, on: bool) -> Self {
        self.rear_defrost = on;
        self
    }

    /// Cabin set-point.
    #[must_use]
    pub fn temperature(&self) -> Celsius {
        self.temperature
    }

    /// How long the engine runs, 1–10 minutes.
    #[must_use]
    pub fn duration_minutes(&self) -> u8 {
        self.duration_minutes
    }

    /// Whether the front windscreen defrost is on.
    #[must_use]
    pub fn defrost(&self) -> bool {
        self.defrost
    }

    /// Whether rear window and mirror heating is on.
    #[must_use]
    pub fn rear_defrost(&self) -> bool {
        self.rear_defrost
    }
}

impl Default for ClimateOptions {
    fn default() -> Self {
        Self::new(Celsius::DEFAULT)
    }
}

/// A place to send to the car's navigation with
/// [`Client::send_destination`](crate::Client::send_destination).
///
/// ```
/// let office = hkconnect::Destination::new("Office", 12.9716, 77.5946)?.with_address("MG Road");
/// # Ok::<(), hkconnect::Error>(())
/// ```
///
/// Serializes with the field names `name`, `latitude`, `longitude` and
/// `address`; deserializing validates the coordinates like
/// [`Destination::new`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "DestinationFields")]
pub struct Destination {
    name: String,
    latitude: f64,
    longitude: f64,
    address: String,
}

/// The unvalidated form of a [`Destination`], for deserializing.
#[derive(Deserialize)]
struct DestinationFields {
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    address: String,
}

impl TryFrom<DestinationFields> for Destination {
    type Error = Error;

    fn try_from(f: DestinationFields) -> Result<Self> {
        Ok(Self::new(f.name, f.latitude, f.longitude)?.with_address(f.address))
    }
}

impl Destination {
    /// A named place, in degrees (WGS 84), with an empty address.
    ///
    /// Fails with [`Error::InvalidCoordinates`](crate::Error::InvalidCoordinates)
    /// unless the latitude is within -90..=90 and the longitude within
    /// -180..=180.
    pub fn new(name: impl Into<String>, latitude: f64, longitude: f64) -> Result<Self> {
        if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
            return Err(Error::InvalidCoordinates {
                latitude,
                longitude,
            });
        }
        Ok(Self {
            name: name.into(),
            latitude,
            longitude,
            address: String::new(),
        })
    }

    /// Set the address shown on the car's screen.
    #[must_use]
    pub fn with_address(mut self, address: impl Into<String>) -> Self {
        self.address = address.into();
        self
    }

    /// Name of the place, shown on the car's screen.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Latitude in degrees.
    #[must_use]
    pub fn latitude(&self) -> f64 {
        self.latitude
    }

    /// Longitude in degrees.
    #[must_use]
    pub fn longitude(&self) -> f64 {
        self.longitude
    }

    /// Address shown on the car's screen; may be empty.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }
}

// ----- wire formats -----
//
// Private mirrors of the API's JSON. Every field is optional and read with
// the lenient helpers in `de.rs`; nested objects of the wrong shape become
// `None`, and list entries that do not parse are skipped.

/// `GET /spa/vehicles`.
#[derive(Debug, Deserialize)]
pub(crate) struct VehicleList {
    #[serde(default, deserialize_with = "lenient_vec")]
    vehicles: Vec<WireVehicle>,
}

impl VehicleList {
    /// The usable vehicles: an entry without an id cannot be addressed, so
    /// it is skipped.
    pub(crate) fn into_vehicles(self) -> Vec<Vehicle> {
        self.vehicles
            .into_iter()
            .filter_map(WireVehicle::into_vehicle)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct WireVehicle {
    #[serde(rename = "vehicleId", default, deserialize_with = "flexible_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "flexible_string")]
    vin: Option<String>,
    #[serde(default, deserialize_with = "flexible_string")]
    nickname: Option<String>,
    #[serde(rename = "vehicleName", default, deserialize_with = "flexible_string")]
    model: Option<String>,
    #[serde(default, deserialize_with = "flexible_string")]
    year: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "flexible_string")]
    powertrain: Option<String>,
    #[serde(
        rename = "ccuCCS2ProtocolSupport",
        default,
        deserialize_with = "flexible_bool_or_false"
    )]
    ccs2: bool,
}

impl WireVehicle {
    fn into_vehicle(self) -> Option<Vehicle> {
        let Some(id) = self.id.filter(|id| !id.trim().is_empty()) else {
            tracing::debug!("skipping a vehicle without an id");
            return None;
        };
        let model = self.model.unwrap_or_default();
        Some(Vehicle {
            id,
            vin: self.vin.unwrap_or_default(),
            name: self
                .nickname
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| model.clone()),
            model,
            year: self.year,
            powertrain: Powertrain::from_code(self.powertrain.as_deref()),
            ccs2: self.ccs2,
        })
    }
}

/// `GET /vehicles/{id}/status[/latest]`.
#[derive(Debug, Deserialize)]
pub(crate) struct WireStatus {
    #[serde(default, deserialize_with = "ist_timestamp")]
    time: Option<Timestamp>,
    #[serde(default, deserialize_with = "flexible_bool")]
    engine: Option<bool>,
    #[serde(rename = "airCtrlOn", default, deserialize_with = "flexible_bool")]
    air_ctrl_on: Option<bool>,
    #[serde(
        rename = "airTemp",
        default,
        deserialize_with = "temperature::from_air_temp"
    )]
    air_temp: Option<Celsius>,
    #[serde(default, deserialize_with = "flexible_bool")]
    defrost: Option<bool>,
    #[serde(rename = "doorLock", default, deserialize_with = "flexible_bool")]
    door_lock: Option<bool>,
    #[serde(rename = "doorOpen", default, deserialize_with = "lenient")]
    door_open: Option<WireDoors>,
    #[serde(rename = "trunkOpen", default, deserialize_with = "flexible_bool")]
    trunk_open: Option<bool>,
    #[serde(rename = "hoodOpen", default, deserialize_with = "flexible_bool")]
    hood_open: Option<bool>,
    #[serde(rename = "sunroofOpen", default, deserialize_with = "flexible_bool")]
    sunroof_open: Option<bool>,
    #[serde(rename = "fuelLevel", default, deserialize_with = "flexible_f64")]
    fuel_level: Option<f64>,
    #[serde(default, deserialize_with = "lenient")]
    dte: Option<WireDistance>,
    #[serde(rename = "lowFuelLight", default, deserialize_with = "flexible_bool")]
    low_fuel_light: Option<bool>,
    #[serde(
        rename = "tirePressureLamp",
        default,
        deserialize_with = "tyre_warning"
    )]
    tire_pressure_lamp: Option<bool>,
    #[serde(
        rename = "washerFluidStatus",
        default,
        deserialize_with = "flexible_bool"
    )]
    washer_fluid_status: Option<bool>,
    #[serde(rename = "breakOilStatus", default, deserialize_with = "flexible_bool")]
    break_oil_status: Option<bool>,
    #[serde(
        rename = "smartKeyBatteryWarning",
        default,
        deserialize_with = "flexible_bool"
    )]
    smart_key_battery_warning: Option<bool>,
    #[serde(rename = "hazardStatus", default, deserialize_with = "flexible_bool")]
    hazard_status: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    battery: Option<WireBattery>,
}

impl WireStatus {
    /// Convert, cleaning up values that are misleading as sent: while
    /// climate is off the car reports a set-point of `"00H"` (14 °C), which
    /// means nothing.
    pub(crate) fn into_status(self) -> VehicleStatus {
        let climate_on = self.air_ctrl_on;
        VehicleStatus {
            updated_at: self.time,
            engine_on: self.engine,
            climate_on,
            climate_temperature: self.air_temp.filter(|_| climate_on == Some(true)),
            defrost_on: self.defrost,
            locked: self.door_lock,
            doors_open: self.door_open.map(WireDoors::into_doors),
            trunk_open: self.trunk_open,
            hood_open: self.hood_open,
            sunroof_open: self.sunroof_open,
            fuel_percent: self.fuel_level,
            range: self.dte.map(WireDistance::into_distance),
            low_fuel_warning: self.low_fuel_light,
            tyre_pressure_warning: self.tire_pressure_lamp,
            washer_fluid_low: self.washer_fluid_status,
            brake_fluid_warning: self.break_oil_status,
            smart_key_battery_low: self.smart_key_battery_warning,
            hazard_lights_on: self.hazard_status,
            battery_12v: self.battery.map(WireBattery::into_battery),
        }
    }
}

/// `{"tirePressureLampAll": 0, ...}`: the overall tyre warning lamp.
fn tyre_warning<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    nested_flag(d, "tirePressureLampAll")
}

/// `{"batWarning": 0}`: the 12V battery warning.
fn battery_warning<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    nested_flag(d, "batWarning")
}

/// A flag nested one level down in an object; `None` for any other shape.
fn nested_flag<'de, D: serde::Deserializer<'de>>(
    d: D,
    key: &str,
) -> Result<Option<bool>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.as_ref().and_then(|v| v.get(key)).and_then(de::truthy))
}

#[derive(Debug, Deserialize)]
#[expect(clippy::struct_excessive_bools, reason = "one flag per door")]
struct WireDoors {
    #[serde(
        rename = "frontLeft",
        default,
        deserialize_with = "flexible_bool_or_false"
    )]
    front_left: bool,
    #[serde(
        rename = "frontRight",
        default,
        deserialize_with = "flexible_bool_or_false"
    )]
    front_right: bool,
    #[serde(
        rename = "backLeft",
        default,
        deserialize_with = "flexible_bool_or_false"
    )]
    back_left: bool,
    #[serde(
        rename = "backRight",
        default,
        deserialize_with = "flexible_bool_or_false"
    )]
    back_right: bool,
}

impl WireDoors {
    fn into_doors(self) -> Doors {
        Doors {
            front_left: self.front_left,
            front_right: self.front_right,
            rear_left: self.back_left,
            rear_right: self.back_right,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireDistance {
    #[serde(default, deserialize_with = "flexible_f64")]
    value: Option<f64>,
    #[serde(default, deserialize_with = "flexible_int")]
    unit: Option<i64>,
}

impl WireDistance {
    fn into_distance(self) -> Distance {
        Distance {
            value: self.value,
            unit: DistanceUnit::from_code(self.unit),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireBattery {
    #[serde(rename = "batSoc", default, deserialize_with = "flexible_int")]
    bat_soc: Option<u8>,
    #[serde(
        rename = "batSignalReferenceValue",
        default,
        deserialize_with = "battery_warning"
    )]
    warning: Option<bool>,
}

impl WireBattery {
    fn into_battery(self) -> Battery12V {
        Battery12V {
            percent: self.bat_soc,
            warning: self.warning,
        }
    }
}

/// `GET /vehicles/{id}/location/park`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawLocation {
    #[serde(default, deserialize_with = "lenient")]
    coord: Option<RawCoord>,
    #[serde(default, deserialize_with = "lenient")]
    speed: Option<RawSpeed>,
    #[serde(default, deserialize_with = "ist_timestamp")]
    time: Option<Timestamp>,
}

#[derive(Debug, Deserialize)]
struct RawCoord {
    #[serde(default, deserialize_with = "flexible_f64")]
    lat: Option<f64>,
    #[serde(default, deserialize_with = "flexible_f64")]
    lon: Option<f64>,
    #[serde(default, deserialize_with = "flexible_f64")]
    alt: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawSpeed {
    #[serde(default, deserialize_with = "flexible_f64")]
    value: Option<f64>,
}

impl RawLocation {
    /// `None` unless both latitude and longitude were sent.
    pub(crate) fn into_location(self) -> Option<Location> {
        let coord = self.coord?;
        Some(Location {
            latitude: coord.lat?,
            longitude: coord.lon?,
            altitude_m: coord.alt,
            speed: self.speed.and_then(|s| s.value),
            updated_at: self.time,
        })
    }
}

/// `GET /vehicles/{id}/setting/alert/maintenance`.
#[derive(Debug, Deserialize)]
pub(crate) struct WireMaintenance {
    #[serde(default, deserialize_with = "flexible_f64")]
    odometer: Option<f64>,
    #[serde(rename = "mainList", default, deserialize_with = "lenient_vec")]
    main_list: Vec<WireMaintenanceItem>,
}

impl WireMaintenance {
    pub(crate) fn into_maintenance(self) -> Maintenance {
        Maintenance {
            odometer_km: self.odometer,
            items: self
                .main_list
                .into_iter()
                .filter_map(WireMaintenanceItem::into_item)
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireMaintenanceItem {
    #[serde(rename = "itemId", default, deserialize_with = "flexible_string")]
    item_id: Option<String>,
    #[serde(default, deserialize_with = "flexible_bool_or_false")]
    enable: bool,
    #[serde(default, deserialize_with = "lenient")]
    items: Option<WireInterval>,
}

impl WireMaintenanceItem {
    /// `None` for an item without a name, which could not be shown.
    fn into_item(self) -> Option<MaintenanceItem> {
        let interval = self
            .items
            .map_or_else(ServiceInterval::default, |i| ServiceInterval {
                km: i.dist_value,
                months: i.term_value,
            });
        Some(MaintenanceItem {
            name: self.item_id?,
            enabled: self.enable,
            interval,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WireInterval {
    #[serde(rename = "distValue", default, deserialize_with = "flexible_int")]
    dist_value: Option<u32>,
    #[serde(rename = "termValue", default, deserialize_with = "flexible_int")]
    term_value: Option<u32>,
}

/// `GET /notifications/{id}/records`: a list of command records. Entries
/// that do not parse are skipped.
#[derive(Debug)]
pub(crate) struct CommandRecords(pub Vec<CommandRecord>);

impl<'de> Deserialize<'de> for CommandRecords {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        lenient_vec(d).map(Self)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommandRecord {
    /// A string or a number, depending on the server; compared as a string.
    #[serde(rename = "recordId", default, deserialize_with = "flexible_string")]
    record_id: Option<String>,
    #[serde(default, deserialize_with = "flexible_string")]
    result: Option<String>,
}

impl CommandRecords {
    /// The status of `command`, or [`CommandStatus::Unknown`] if it is not
    /// listed.
    pub(crate) fn status_of(&self, command: &CommandId) -> CommandStatus {
        self.0
            .iter()
            .find(|r| r.record_id.as_deref() == Some(command.as_str()))
            .map_or(CommandStatus::Unknown, CommandRecord::status)
    }
}

impl CommandRecord {
    fn status(&self) -> CommandStatus {
        match self.result.as_deref() {
            None => CommandStatus::Pending,
            Some("success") => CommandStatus::Succeeded,
            Some("fail") => CommandStatus::Failed,
            Some("non-response") => CommandStatus::NoResponse,
            Some(_) => CommandStatus::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn records(v: Value) -> CommandRecords {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn record_ids_compare_as_strings() {
        let r = records(json!([
            "junk",
            { "recordId": 42, "result": "success" },
            { "recordId": "abc", "result": null },
        ]));
        assert_eq!(r.status_of(&CommandId::new("42")), CommandStatus::Succeeded);
        assert_eq!(r.status_of(&CommandId::new("abc")), CommandStatus::Pending);
        assert_eq!(r.status_of(&CommandId::new("x")), CommandStatus::Unknown);
    }

    #[test]
    fn climate_duration_is_validated() {
        let options = ClimateOptions::default();
        assert_eq!(options.duration_minutes(), 10);
        assert_eq!(options.temperature(), Celsius::new(22.0).unwrap());
        for bad in [0, 11, 255] {
            assert!(matches!(
                options.with_duration_minutes(bad),
                Err(Error::InvalidDuration(m)) if m == bad
            ));
        }
        assert_eq!(
            options.with_duration_minutes(1).unwrap().duration_minutes(),
            1
        );
    }

    #[test]
    fn destination_coordinates_are_validated() {
        for (lat, lon) in [
            (91.0, 0.0),
            (0.0, -180.5),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(
                matches!(
                    Destination::new("x", lat, lon),
                    Err(Error::InvalidCoordinates { .. })
                ),
                "{lat}, {lon}"
            );
        }
        let d = Destination::new("x", -90.0, 180.0).unwrap();
        assert_eq!(d.address(), "");
        let bad = json!({ "name": "x", "latitude": 100.0, "longitude": 0.0 });
        assert!(serde_json::from_value::<Destination>(bad).is_err());
    }
}
