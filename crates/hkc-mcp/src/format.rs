//! Turning library types into compact JSON for an LLM caller.
//!
//! Times are shown in Indian Standard Time (the car's and the owner's zone),
//! and anything cached carries an explicit age so it is never mistaken for
//! live data. Fields the car did not report are left out rather than sent as
//! `null`.

use hkconnect::{
    Capabilities, DistanceUnit, Location, Maintenance, Trip, TripDay, TripDetail, TripMonth,
    TripTotals, Vehicle, VehicleStatus,
};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::{Offset, TimeZone};
use serde_json::{Map, Value, json};

/// Most route points returned for one trip; longer routes are thinned
/// evenly (first and last points are always kept).
pub const MAX_ROUTE_POINTS: usize = 400;

/// Most trips whose detail is fetched for one `with_events` day listing.
pub const MAX_EVENT_TRIPS: usize = 20;

/// Indian Standard Time, UTC+05:30.
pub fn ist() -> TimeZone {
    TimeZone::fixed(Offset::from_seconds(5 * 3600 + 30 * 60).expect("valid offset"))
}

/// Today's date in India.
pub fn india_today(now: Timestamp) -> Date {
    now.to_zoned_datetime(ist()).date()
}

/// `2026-11-23 16:11 IST`.
pub fn ist_time(ts: Timestamp) -> String {
    ts.to_zoned_datetime(ist())
        .strftime("%Y-%m-%d %H:%M IST")
        .to_string()
}

/// `16:11`.
fn ist_clock(ts: Timestamp) -> String {
    ts.to_zoned_datetime(ist()).strftime("%H:%M").to_string()
}

/// How long ago, for sentences: `just now`, `12 min ago`, `3 h 5 min ago`.
pub fn ago(now: Timestamp, then: Timestamp) -> String {
    match age(now, then) {
        a if a == "just now" => a,
        a => format!("{a} ago"),
    }
}

/// A short human age: `just now`, `12 min`, `3 h 5 min`, `2 days`.
pub fn age(now: Timestamp, then: Timestamp) -> String {
    let secs = (now - then).as_secs().max(0);
    let mins = secs / 60;
    match mins {
        0 => "just now".into(),
        1..60 => format!("{mins} min"),
        60..2880 => {
            let (h, m) = (mins / 60, mins % 60);
            if m == 0 {
                format!("{h} h")
            } else {
                format!("{h} h {m} min")
            }
        }
        _ => format!("{} days", mins / 1440),
    }
}

/// Insert `key` only when `value` is not null.
fn put(map: &mut Map<String, Value>, key: &str, value: impl Into<Value>) {
    let value = value.into();
    if !value.is_null() {
        map.insert(key.into(), value);
    }
}

/// Round to `places` decimals for compact output.
fn round(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

/// The fields every timestamped reading carries: `as_of`, `data_age`,
/// `data_age_seconds`.
fn stamp(map: &mut Map<String, Value>, now: Timestamp, at: Option<Timestamp>) {
    match at {
        Some(at) => {
            put(map, "as_of", ist_time(at));
            put(map, "data_age", age(now, at));
            put(map, "data_age_seconds", (now - at).as_secs().max(0));
        }
        None => put(map, "as_of", "unknown"),
    }
}

/// A car, for listings and for naming the car a result is about.
pub fn vehicle(v: &Vehicle) -> Value {
    let mut m = Map::new();
    put(&mut m, "name", v.name.clone());
    put(&mut m, "model", v.model.clone());
    put(&mut m, "year", v.year.clone());
    put(&mut m, "vin", v.vin.clone());
    put(&mut m, "id", v.id.clone());
    put(
        &mut m,
        "powertrain",
        serde_json::to_value(v.powertrain).ok(),
    );
    if v.ccs2 {
        put(
            &mut m,
            "note",
            "newer CCS2 car: status and remote commands are not supported",
        );
    }
    Value::Object(m)
}

/// A status reading. `live` says whether it was just fetched from the car.
pub fn status(car: &Vehicle, s: &VehicleStatus, live: bool, now: Timestamp) -> Value {
    let mut m = Map::new();
    put(&mut m, "vehicle", car.name.clone());
    put(
        &mut m,
        "source",
        if live {
            "live (car was woken just now)"
        } else {
            "cached (last report from the car; may be old)"
        },
    );
    stamp(&mut m, now, s.updated_at);
    put(&mut m, "locked", s.locked);
    let mut open = Vec::new();
    if let Some(d) = s.doors_open {
        for (is_open, name) in [
            (d.front_left, "front left door"),
            (d.front_right, "front right door"),
            (d.rear_left, "rear left door"),
            (d.rear_right, "rear right door"),
        ] {
            if is_open {
                open.push(name);
            }
        }
    }
    for (flag, name) in [
        (s.trunk_open, "boot"),
        (s.hood_open, "bonnet"),
        (s.sunroof_open, "sunroof"),
    ] {
        if flag == Some(true) {
            open.push(name);
        }
    }
    if s.doors_open.is_some() || s.trunk_open.is_some() || s.hood_open.is_some() {
        put(&mut m, "open", open);
    }
    put(&mut m, "engine_on", s.engine_on);
    put(&mut m, "climate_on", s.climate_on);
    put(
        &mut m,
        "climate_set_c",
        s.climate_temperature.map(|t| f64::from(t.degrees())),
    );
    put(&mut m, "defrost_on", s.defrost_on);
    put(&mut m, "fuel_percent", s.fuel_percent.map(|f| round(f, 0)));
    if let Some(r) = s.range
        && let Some(v) = r.value
    {
        let unit = match r.unit {
            DistanceUnit::Miles => "range_miles",
            _ => "range_km",
        };
        put(&mut m, unit, round(v, 0));
    }
    put(&mut m, "hazard_lights_on", s.hazard_lights_on);
    if let Some(b) = s.battery_12v {
        put(&mut m, "battery_12v_percent", b.percent);
    }
    let warnings: Vec<&str> = [
        (s.low_fuel_warning, "low fuel"),
        (s.tyre_pressure_warning, "tyre pressure"),
        (s.washer_fluid_low, "washer fluid low"),
        (s.brake_fluid_warning, "brake fluid"),
        (s.smart_key_battery_low, "smart key battery low"),
        (s.battery_12v.and_then(|b| b.warning), "12V battery"),
    ]
    .into_iter()
    .filter_map(|(on, name)| (on == Some(true)).then_some(name))
    .collect();
    put(&mut m, "warnings", warnings);
    Value::Object(m)
}

/// One line describing a status reading.
pub fn status_summary(car: &Vehicle, s: &VehicleStatus, live: bool, now: Timestamp) -> String {
    let lock = match s.locked {
        Some(true) => "locked",
        Some(false) => "UNLOCKED",
        None => "lock state unknown",
    };
    let engine = match s.engine_on {
        Some(true) => ", engine running",
        _ => "",
    };
    let open = if s.anything_open() {
        ", something is open"
    } else {
        ""
    };
    let fuel = s
        .fuel_percent
        .map(|f| format!(", fuel {f:.0}%"))
        .unwrap_or_default();
    let when = match s.updated_at {
        Some(at) if live => format!("live as of {}", ist_time(at)),
        Some(at) => format!("cached, as of {} ({})", ist_time(at), ago(now, at)),
        None if live => "live".into(),
        None => "cached, time unknown".into(),
    };
    format!("{}: {lock}{engine}{open}{fuel} — {when}.", car.name)
}

/// Where the car was parked.
pub fn location(car: &Vehicle, l: &Location, now: Timestamp) -> Value {
    let mut m = Map::new();
    put(&mut m, "vehicle", car.name.clone());
    put(&mut m, "latitude", l.latitude);
    put(&mut m, "longitude", l.longitude);
    stamp(&mut m, now, l.updated_at);
    put(&mut m, "map_url", map_url(l.latitude, l.longitude));
    put(
        &mut m,
        "note",
        "this is where the car last reported its parked position, not a live GPS track",
    );
    Value::Object(m)
}

/// A Google Maps link for a point.
pub fn map_url(lat: f64, lon: f64) -> String {
    format!("https://www.google.com/maps?q={lat},{lon}")
}

/// Equipment, odometer and service intervals.
pub fn info(car: &Vehicle, caps: &Capabilities, m: &Maintenance) -> Value {
    let mut out = Map::new();
    put(&mut out, "vehicle", vehicle(car));
    put(&mut out, "model", caps.model.clone());
    put(&mut out, "model_year", caps.model_year.clone());
    put(&mut out, "transmission", caps.transmission.clone());
    put(&mut out, "odometer_km", m.odometer_km.map(|k| round(k, 0)));
    let features: Vec<&str> = [
        (caps.remote_start, "remote start"),
        (caps.horn_and_lights, "horn and lights"),
        (caps.lights_only, "lights only"),
        (caps.rear_defrost, "rear defrost"),
        (caps.steering_wheel_heat, "heated steering wheel"),
        (caps.seat_ventilation, "ventilated seats"),
    ]
    .into_iter()
    .filter_map(|(has, name)| has.then_some(name))
    .collect();
    put(&mut out, "remote_features", features);
    put(
        &mut out,
        "connectivity_ends",
        caps.connectivity_ends.clone(),
    );
    let intervals: Vec<Value> = m
        .items
        .iter()
        .filter(|i| i.enabled)
        .map(|i| {
            let mut item = Map::new();
            put(&mut item, "item", i.name.clone());
            put(&mut item, "every_km", i.interval.km);
            put(
                &mut item,
                "every_months",
                i.interval.months.filter(|&n| n > 0),
            );
            Value::Object(item)
        })
        .collect();
    put(&mut out, "service_intervals", intervals);
    Value::Object(out)
}

fn totals(t: &TripTotals) -> Value {
    let mut m = Map::new();
    put(&mut m, "distance_km", t.distance_km.map(|d| round(d, 1)));
    put(&mut m, "drive_minutes", t.drive_minutes);
    put(&mut m, "idle_minutes", t.idle_minutes);
    put(
        &mut m,
        "average_speed_kmh",
        t.average_speed_kmh.map(|s| round(s, 1)),
    );
    put(
        &mut m,
        "max_speed_kmh",
        t.max_speed_kmh.map(|s| round(s, 0)),
    );
    Value::Object(m)
}

/// A month's trip summary.
pub fn trip_month(car: &Vehicle, year: i16, month: i8, m: &TripMonth) -> Value {
    let mut days: Vec<_> = m.days.clone();
    days.sort_by_key(|d| d.date);
    json!({
        "vehicle": car.name,
        "month": format!("{year:04}-{month:02}"),
        "totals": totals(&m.totals),
        "days_with_trips": days
            .iter()
            .map(|d| json!({ "date": d.date.to_string(), "trips": d.trips }))
            .collect::<Vec<_>>(),
        "hint": "call get_trips with date=YYYY-MM-DD to list one day's trips",
    })
}

fn trip_entry(n: usize, t: &Trip, detail: Option<&TripDetail>) -> Value {
    let mut m = Map::new();
    put(&mut m, "trip", n);
    put(&mut m, "start", t.started_at.map(ist_clock));
    put(&mut m, "end", t.ended_at.map(ist_clock));
    if let (Some(a), Some(b)) = (t.started_at, t.ended_at) {
        put(&mut m, "minutes", (b - a).as_secs().max(0) / 60);
    }
    if let Some(c) = t.start {
        put(&mut m, "from", json!([c.latitude, c.longitude]));
    }
    if let Some(c) = t.end {
        put(&mut m, "to", json!([c.latitude, c.longitude]));
    }
    if let Some(d) = detail {
        put(
            &mut m,
            "distance_km",
            d.totals.distance_km.map(|v| round(v, 1)),
        );
        put(
            &mut m,
            "max_speed_kmh",
            d.totals.max_speed_kmh.map(|v| round(v, 0)),
        );
        events(&mut m, d);
    }
    Value::Object(m)
}

/// The harsh braking, rapid acceleration and speeding counts of a trip.
fn events(m: &mut Map<String, Value>, d: &TripDetail) {
    put(m, "harsh_braking_events", d.hard_braking);
    put(m, "rapid_acceleration_events", d.rapid_acceleration);
    put(m, "speeding_events", d.speeding);
}

/// Driving events fetched for a day's trips (`with_events`): `details[i]` is
/// trip `i + 1` in time order; trips past the end were not fetched.
#[derive(Debug, Default)]
pub struct DayEvents {
    /// Details of the first trips of the day, in time order.
    pub details: Vec<TripDetail>,
    /// Why fetching stopped early, if a request failed.
    pub error: Option<String>,
}

/// The trips on one day, numbered from 1 in time order. With `events`, the
/// trips it covers also carry their distance, top speed and driving events.
pub fn trip_day(car: &Vehicle, d: &TripDay, events: Option<&DayEvents>) -> Value {
    let trips = sorted_trips(d);
    let details = events.map_or(&[][..], |e| e.details.as_slice());
    let mut m = Map::new();
    put(&mut m, "vehicle", car.name.clone());
    put(&mut m, "date", d.date.to_string());
    put(&mut m, "times", "IST");
    put(&mut m, "totals", totals(&d.totals));
    put(
        &mut m,
        "trips",
        trips
            .iter()
            .enumerate()
            .map(|(i, t)| trip_entry(i + 1, t, details.get(i)))
            .collect::<Vec<_>>(),
    );
    let Some(events) = events else {
        put(
            &mut m,
            "hint",
            "call get_trips with the same date and trip=N for one trip's driving events, \
             or with_events=true for every trip's events in one call",
        );
        return Value::Object(m);
    };
    let sum = |f: fn(&TripDetail) -> Option<u32>| -> Option<u32> {
        details.iter().filter_map(f).reduce(|a, b| a + b)
    };
    let mut t = Map::new();
    put(&mut t, "trips_counted", details.len());
    put(&mut t, "harsh_braking_events", sum(|d| d.hard_braking));
    put(
        &mut t,
        "rapid_acceleration_events",
        sum(|d| d.rapid_acceleration),
    );
    put(&mut t, "speeding_events", sum(|d| d.speeding));
    put(
        &mut t,
        "max_speed_kmh",
        details
            .iter()
            .filter_map(|d| d.totals.max_speed_kmh)
            .reduce(f64::max)
            .map(|v| round(v, 0)),
    );
    put(&mut m, "events_totals", Value::Object(t));
    if let Some(e) = &events.error {
        put(
            &mut m,
            "events_error",
            format!(
                "{e}. Events are missing for trip {} and later; get them with trip=N.",
                details.len() + 1
            ),
        );
    } else if details.len() < trips.len() {
        put(
            &mut m,
            "events_truncated",
            format!(
                "events cover trips 1 to {} of {} (at most {MAX_EVENT_TRIPS} per call); \
                 get the rest with trip=N",
                details.len(),
                trips.len()
            ),
        );
    }
    put(
        &mut m,
        "hint",
        "call get_trips with the same date and trip=N for one trip's full detail; \
         add include_route=true only to show or map its path",
    );
    Value::Object(m)
}

/// The day's trips in time order (the server's order is not guaranteed).
pub fn sorted_trips(d: &TripDay) -> Vec<&Trip> {
    let mut trips: Vec<&Trip> = d.trips.iter().collect();
    trips.sort_by_key(|t| t.started_at);
    trips
}

/// One trip's detail: totals and driving events, plus with `include_route`
/// the route thinned to at most [`MAX_ROUTE_POINTS`] `[lat, lon]` pairs.
/// Without it the route is left out (it is most of the size) and a hint says
/// how to get it.
pub fn trip_detail(
    car: &Vehicle,
    date: Date,
    n: u32,
    d: &TripDetail,
    include_route: bool,
) -> Value {
    let mut m = Map::new();
    put(&mut m, "vehicle", car.name.clone());
    put(&mut m, "date", date.to_string());
    put(&mut m, "trip", n);
    put(&mut m, "start", d.started_at.map(ist_time));
    put(&mut m, "end", d.ended_at.map(ist_time));
    put(&mut m, "totals", totals(&d.totals));
    events(&mut m, d);
    put(&mut m, "route_points_total", d.route.len());
    if include_route {
        let route = thin(&d.route, MAX_ROUTE_POINTS);
        if route.len() < d.route.len() {
            put(&mut m, "route_points_shown", route.len());
        }
        put(
            &mut m,
            "route",
            route
                .iter()
                .map(|p| json!([round(p.latitude, 5), round(p.longitude, 5)]))
                .collect::<Vec<_>>(),
        );
    } else if !d.route.is_empty() {
        put(
            &mut m,
            "hint",
            format!(
                "route left out; call again with include_route=true for the GPS path \
                 (up to {MAX_ROUTE_POINTS} points), only when the user wants to see or \
                 map where the car went"
            ),
        );
    }
    if let (Some(first), Some(last)) = (d.route.first(), d.route.last()) {
        put(
            &mut m,
            "from_map_url",
            map_url(first.latitude, first.longitude),
        );
        put(&mut m, "to_map_url", map_url(last.latitude, last.longitude));
    }
    Value::Object(m)
}

/// Keep at most `max` items, evenly spaced, always including the last.
fn thin<T>(items: &[T], max: usize) -> Vec<&T> {
    if items.len() <= max || max < 2 {
        return items.iter().take(max).collect();
    }
    let last = items.len() - 1;
    (0..max).map(|i| &items[i * last / (max - 1)]).collect()
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    #[test]
    fn ages_read_naturally() {
        let now: Timestamp = "2026-11-23T12:00:00Z".parse().unwrap();
        let at = |s: i64| now - SignedDuration::from_secs(s);
        assert_eq!(age(now, at(10)), "just now");
        assert_eq!(ago(now, at(10)), "just now");
        assert_eq!(ago(now, at(600)), "10 min ago");
        assert_eq!(age(now, at(600)), "10 min");
        assert_eq!(age(now, at(3600)), "1 h");
        assert_eq!(age(now, at(3 * 3600 + 300)), "3 h 5 min");
        assert_eq!(age(now, at(3 * 86400)), "3 days");
        // A clock skew into the future is not a negative age.
        assert_eq!(age(now, now + SignedDuration::from_secs(90)), "just now");
    }

    #[test]
    fn times_are_ist() {
        let t: Timestamp = "2026-11-23T10:41:31Z".parse().unwrap();
        assert_eq!(ist_time(t), "2026-11-23 16:11 IST");
        let late: Timestamp = "2026-11-23T19:00:00Z".parse().unwrap();
        assert_eq!(india_today(late).to_string(), "2026-11-24");
    }

    #[test]
    fn thinning_keeps_ends_and_bounds() {
        let v: Vec<u32> = (0..1000).collect();
        let t = thin(&v, 10);
        assert_eq!(t.len(), 10);
        assert_eq!(*t[0], 0);
        assert_eq!(*t[9], 999);
        assert_eq!(thin(&v[..5], 10).len(), 5);
    }
}
