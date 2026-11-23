//! Trip history: monthly summaries, the trips on a day, and one trip's
//! route and driving events.
//!
//! Units as sent by the API: minutes, kilometres, km/h.

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::de::{
    flexible_f64, flexible_int, flexible_string, ist_timestamp, lenient, lenient_vec, parse_ist,
    parse_utc,
};

/// Totals for a month, and which days had trips, from
/// [`Client::trips_in_month`](crate::Client::trips_in_month).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TripMonth {
    /// Driving totals for the whole month.
    pub totals: TripTotals,
    /// The days that had at least one trip, in the order the server sent
    /// them.
    pub days: Vec<TripDayCount>,
}

/// How many trips were taken on one day of a [`TripMonth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TripDayCount {
    /// The day, in Indian Standard Time.
    pub date: Date,
    /// Number of trips that day.
    pub trips: u32,
}

/// Driving totals over some period: a month, a day, or one trip. Each field
/// is `None` if the server did not report it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TripTotals {
    /// Time spent driving, in minutes.
    pub drive_minutes: Option<u32>,
    /// Time spent idling with the engine on, in minutes.
    pub idle_minutes: Option<u32>,
    /// Distance driven, in km.
    pub distance_km: Option<f64>,
    /// Average speed, in km/h.
    pub average_speed_kmh: Option<f64>,
    /// Top speed, in km/h.
    pub max_speed_kmh: Option<f64>,
}

/// The trips taken on one day, from
/// [`Client::trips_on_day`](crate::Client::trips_on_day).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TripDay {
    /// The requested day, in Indian Standard Time.
    pub date: Date,
    /// Driving totals for the day. All `None` if there were no trips.
    pub totals: TripTotals,
    /// The trips, oldest first.
    pub trips: Vec<Trip>,
}

/// One trip, as listed for a day. Pass it to
/// [`Client::trip_detail`](crate::Client::trip_detail) for the route.
///
/// A `Trip` can be serialized, stored, deserialized and passed to
/// [`Client::trip_detail`](crate::Client::trip_detail) later. Its serialized
/// form includes an opaque `_detail_ref` object holding the server's own
/// identifiers for the trip, echoed back exactly as received; treat it as
/// private and keep it unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Trip {
    /// The server's id for this trip (its `serviceTID`), as text.
    pub id: String,
    /// When the trip started; `None` if the server sent an unparseable time.
    pub started_at: Option<Timestamp>,
    /// When the trip ended; `None` if the server sent an unparseable time.
    pub ended_at: Option<Timestamp>,
    /// Where the trip started, if reported.
    pub start: Option<Coordinates>,
    /// Where the trip ended, if reported.
    pub end: Option<Coordinates>,
    /// What [`Client::trip_detail`](crate::Client::trip_detail) sends back.
    #[serde(rename = "_detail_ref")]
    pub(crate) detail_ref: TripRef,
}

/// The raw identifiers the API uses for a trip, echoed back verbatim when
/// asking for its detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TripRef {
    /// `serviceTID`, as received: the server has sent it as a number, but
    /// it is echoed with whatever JSON type it arrived as.
    pub service_tid: Value,
    /// `tripStartTime`, local time `YYYYMMDDHHMMSS`.
    pub start: String,
    /// `tripEndTime`, local time `YYYYMMDDHHMMSS`.
    pub end: String,
}

impl TripRef {
    /// The `YYYYMMDD` day the trip is listed under: the start time's date.
    pub(crate) fn day(&self) -> &str {
        self.start.get(..8).unwrap_or(&self.start)
    }
}

/// One trip's route and driving behaviour, from
/// [`Client::trip_detail`](crate::Client::trip_detail).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TripDetail {
    /// When the trip started; `None` if not reported.
    pub started_at: Option<Timestamp>,
    /// When the trip ended; `None` if not reported.
    pub ended_at: Option<Timestamp>,
    /// Driving totals for this trip.
    pub totals: TripTotals,
    /// Number of harsh-braking events.
    pub hard_braking: Option<u32>,
    /// Number of rapid-acceleration events.
    pub rapid_acceleration: Option<u32>,
    /// Number of speeding events.
    pub speeding: Option<u32>,
    /// GPS points along the route, in the order the server sent them.
    pub route: Vec<RoutePoint>,
}

/// A position (WGS 84).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Coordinates {
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
}

/// One GPS point on a trip's route.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoutePoint {
    /// When the car was here; `None` if not reported.
    pub at: Option<Timestamp>,
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
}

// ----- wire formats -----

#[derive(Debug, Default, Deserialize)]
struct WireTotals {
    #[serde(rename = "tripDrvTime", default, deserialize_with = "flexible_int")]
    drive_minutes: Option<u32>,
    #[serde(rename = "tripIdleTime", default, deserialize_with = "flexible_int")]
    idle_minutes: Option<u32>,
    #[serde(rename = "tripDist", default, deserialize_with = "flexible_f64")]
    distance_km: Option<f64>,
    #[serde(rename = "tripAvgSpeed", default, deserialize_with = "flexible_f64")]
    average_speed_kmh: Option<f64>,
    #[serde(rename = "tripMaxSpeed", default, deserialize_with = "flexible_f64")]
    max_speed_kmh: Option<f64>,
}

impl From<WireTotals> for TripTotals {
    fn from(t: WireTotals) -> Self {
        Self {
            drive_minutes: t.drive_minutes,
            idle_minutes: t.idle_minutes,
            distance_km: t.distance_km,
            average_speed_kmh: t.average_speed_kmh,
            max_speed_kmh: t.max_speed_kmh,
        }
    }
}

/// `POST /vehicles/{id}/tripinfo` for a month.
#[derive(Debug, Deserialize)]
pub(crate) struct WireTripMonth {
    #[serde(flatten)]
    totals: WireTotals,
    #[serde(rename = "tripDayList", default, deserialize_with = "lenient_vec")]
    days: Vec<WireTripDayCount>,
}

/// An entry without a valid date fails to parse and is skipped.
#[derive(Debug, Deserialize)]
struct WireTripDayCount {
    #[serde(rename = "tripDayInMonth", deserialize_with = "yyyymmdd")]
    date: Date,
    #[serde(rename = "tripCntDay", default, deserialize_with = "flexible_int")]
    trips: Option<u32>,
}

impl WireTripMonth {
    pub(crate) fn into_month(self) -> TripMonth {
        TripMonth {
            totals: self.totals.into(),
            days: self
                .days
                .into_iter()
                .map(|d| TripDayCount {
                    date: d.date,
                    trips: d.trips.unwrap_or(0),
                })
                .collect(),
        }
    }
}

/// `POST /vehicles/{id}/tripinfo` for a day.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTripDayResponse {
    #[serde(rename = "dayTripList", default, deserialize_with = "lenient_vec")]
    day_trip_list: Vec<RawTripDay>,
}

#[derive(Debug, Deserialize)]
struct RawTripDay {
    #[serde(flatten)]
    totals: WireTotals,
    #[serde(rename = "tripList", default, deserialize_with = "lenient_vec")]
    trip_list: Vec<RawTrip>,
}

/// A trip without an id or a start time cannot be looked up later, so it
/// fails to parse and is skipped.
#[derive(Debug, Deserialize)]
struct RawTrip {
    #[serde(rename = "serviceTID", deserialize_with = "present")]
    service_tid: Value,
    #[serde(rename = "tripStartTime", deserialize_with = "required_string")]
    start: String,
    #[serde(rename = "tripEndTime", default, deserialize_with = "flexible_string")]
    end: Option<String>,
    #[serde(rename = "tripStartCoord", default, deserialize_with = "lenient")]
    start_coord: Option<RawCoord>,
    #[serde(rename = "tripEndCoord", default, deserialize_with = "lenient")]
    end_coord: Option<RawCoord>,
}

impl RawTrip {
    fn into_trip(self) -> Trip {
        let id = match &self.service_tid {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let end = self.end.unwrap_or_default();
        Trip {
            id,
            started_at: parse_ist(&self.start),
            ended_at: parse_ist(&end),
            start: self.start_coord.and_then(RawCoord::into_coordinates),
            end: self.end_coord.and_then(RawCoord::into_coordinates),
            detail_ref: TripRef {
                service_tid: self.service_tid,
                start: self.start,
                end,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawCoord {
    #[serde(default, deserialize_with = "flexible_f64")]
    lat: Option<f64>,
    #[serde(default, deserialize_with = "flexible_f64")]
    lon: Option<f64>,
}

impl RawCoord {
    fn into_coordinates(self) -> Option<Coordinates> {
        Some(Coordinates {
            latitude: self.lat?,
            longitude: self.lon?,
        })
    }
}

impl RawTripDayResponse {
    pub(crate) fn into_day(self, date: Date) -> TripDay {
        let mut totals = TripTotals::default();
        let mut trips = Vec::new();
        // Normally one entry per day; merge defensively if there are more.
        for (i, day) in self.day_trip_list.into_iter().enumerate() {
            if i == 0 {
                totals = day.totals.into();
            }
            trips.extend(day.trip_list.into_iter().map(RawTrip::into_trip));
        }
        // The server lists newest first; chronological order is easier to
        // read and makes trip numbers stable.
        trips.sort_by(|a, b| a.detail_ref.start.cmp(&b.detail_ref.start));
        TripDay {
            date,
            totals,
            trips,
        }
    }
}

/// `POST /vehicles/{id}/tripinfo/detail`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTripDetailResponse {
    /// Missing, null or malformed reads as an empty detail.
    #[serde(rename = "tripInfo", default, deserialize_with = "lenient")]
    trip_info: Option<RawTripDetail>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTripDetail {
    #[serde(flatten)]
    totals: WireTotals,
    #[serde(rename = "tripStartTime", default, deserialize_with = "ist_timestamp")]
    started_at: Option<Timestamp>,
    #[serde(rename = "tripEndTime", default, deserialize_with = "ist_timestamp")]
    ended_at: Option<Timestamp>,
    #[serde(
        rename = "tripHardBreakingTime",
        default,
        deserialize_with = "flexible_int"
    )]
    hard_braking: Option<u32>,
    #[serde(
        rename = "tripRapidAccelationTime",
        default,
        deserialize_with = "flexible_int"
    )]
    rapid_acceleration: Option<u32>,
    #[serde(
        rename = "tripExcessiveSpeedTime",
        default,
        deserialize_with = "flexible_int"
    )]
    speeding: Option<u32>,
    #[serde(rename = "tripList", default, deserialize_with = "lenient_vec")]
    route: Vec<RawRoutePoint>,
}

#[derive(Debug, Deserialize)]
struct RawRoutePoint {
    #[serde(default, deserialize_with = "flexible_f64")]
    lat: Option<f64>,
    #[serde(default, deserialize_with = "flexible_f64")]
    lon: Option<f64>,
    #[serde(rename = "tripDate", default, deserialize_with = "lenient")]
    date: Option<RawUtc>,
}

#[derive(Debug, Deserialize)]
struct RawUtc {
    #[serde(default, deserialize_with = "flexible_string")]
    utc: Option<String>,
}

impl RawRoutePoint {
    /// `None` for a point without both latitude and longitude.
    fn into_point(self) -> Option<RoutePoint> {
        Some(RoutePoint {
            at: self.date.and_then(|d| d.utc).as_deref().and_then(parse_utc),
            latitude: self.lat?,
            longitude: self.lon?,
        })
    }
}

impl RawTripDetailResponse {
    pub(crate) fn into_detail(self) -> TripDetail {
        let t = self.trip_info.unwrap_or_default();
        TripDetail {
            started_at: t.started_at,
            ended_at: t.ended_at,
            totals: t.totals.into(),
            hard_braking: t.hard_braking,
            rapid_acceleration: t.rapid_acceleration,
            speeding: t.speeding,
            route: t
                .route
                .into_iter()
                .filter_map(RawRoutePoint::into_point)
                .collect(),
        }
    }
}

fn yyyymmdd<'de, D: Deserializer<'de>>(d: D) -> Result<Date, D::Error> {
    let raw = String::deserialize(d)?;
    Date::strptime("%Y%m%d", &raw).map_err(serde::de::Error::custom)
}

/// Any value except null.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Err(serde::de::Error::custom("unexpected null")),
        v => Ok(v),
    }
}

/// A string or number, converted to a string; null is an error.
fn required_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    flexible_string(d)?.ok_or_else(|| serde::de::Error::custom("expected a string"))
}
