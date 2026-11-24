//! Human and JSON output.

use std::time::Duration;

use anyhow::Result;
use hkconnect::{
    Capabilities, CommandId, CommandStatus, Location, Maintenance, TripDay, TripDetail, TripMonth,
    Vehicle, VehicleStatus,
};
use jiff::{Timestamp, tz::TimeZone};
use serde::Serialize;
use serde_json::json;

/// Make a string from the server (a car's name, a maintenance item, an
/// error message) safe to print on a terminal: control characters other
/// than newline and tab, which could start escape sequences, and Unicode
/// bidirectional overrides, which can reorder what is shown, are replaced
/// with U+FFFD.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi = matches!(
                c,
                '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            );
            if (c.is_control() && c != '\n' && c != '\t') || bidi {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// An error and its causes, joined with ": " (as `anyhow`'s `{:#}` does).
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

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn row(label: &str, value: impl std::fmt::Display) {
    println!("{label:<18} {}", clean(&value.to_string()));
}

fn yes_no(value: Option<bool>, yes: &str, no: &str) -> String {
    match value {
        Some(true) => yes.into(),
        Some(false) => no.into(),
        None => "unknown".into(),
    }
}

/// "23 Nov 16:11 (2 h ago)" in the machine's local time zone.
fn when(ts: Option<Timestamp>) -> String {
    let Some(ts) = ts else {
        return "unknown".into();
    };
    let local = ts.to_zoned_datetime(TimeZone::system());
    let mins = (Timestamp::now() - ts).as_secs().max(0) / 60;
    let ago = match mins {
        0 => "just now".into(),
        1..60 => format!("{mins} min ago"),
        60..2880 => format!("{} h ago", mins / 60),
        _ => format!("{} days ago", mins / 1440),
    };
    format!("{} ({ago})", local.strftime("%d %b %H:%M"))
}

pub fn vehicles(vehicles: &[Vehicle], json: bool) -> Result<()> {
    if json {
        return print_json(&vehicles);
    }
    for v in vehicles {
        let year = v.year.as_deref().unwrap_or("");
        println!(
            "{}",
            clean(&format!("{}  {} {year}  VIN {}", v.name, v.model, v.vin))
        );
    }
    Ok(())
}

pub fn status(car: &Vehicle, s: &VehicleStatus, json: bool) -> Result<()> {
    if json {
        return print_json(s);
    }
    println!("{} — as of {}", clean(&car.name), when(s.updated_at));
    row("Doors", yes_no(s.locked, "locked", "UNLOCKED"));
    if s.anything_open() {
        let d = s.doors_open.unwrap_or_default();
        let open: Vec<&str> = [
            (d.front_left, "front left"),
            (d.front_right, "front right"),
            (d.rear_left, "rear left"),
            (d.rear_right, "rear right"),
            (s.trunk_open == Some(true), "boot"),
            (s.hood_open == Some(true), "bonnet"),
        ]
        .into_iter()
        .filter_map(|(open, name)| open.then_some(name))
        .collect();
        row("Open", open.join(", "));
    } else {
        row("Open", "nothing");
    }
    row("Engine", yes_no(s.engine_on, "running", "off"));
    let climate = match (s.climate_on, s.climate_temperature) {
        (Some(true), Some(t)) => format!("on, {t}"),
        (other, _) => yes_no(other, "on", "off"),
    };
    row("Climate", climate);
    if let Some(fuel) = s.fuel_percent {
        let range = s
            .range
            .and_then(|r| {
                r.value
                    .map(|v| format!(", {v:.0} {:?} range", r.unit).to_lowercase())
            })
            .unwrap_or_default();
        row("Fuel", format!("{fuel:.0}%{range}"));
    }
    let warnings: Vec<&str> = [
        (s.low_fuel_warning, "low fuel"),
        (s.tyre_pressure_warning, "tyre pressure"),
        (s.washer_fluid_low, "washer fluid"),
        (s.brake_fluid_warning, "brake fluid"),
        (s.smart_key_battery_low, "key battery"),
        (s.battery_12v.and_then(|b| b.warning), "12V battery"),
    ]
    .into_iter()
    .filter_map(|(on, name)| (on == Some(true)).then_some(name))
    .collect();
    row(
        "Warnings",
        if warnings.is_empty() {
            "none".into()
        } else {
            warnings.join(", ")
        },
    );
    Ok(())
}

pub fn location(location: Option<&Location>, json: bool) -> Result<()> {
    if json {
        return print_json(&location);
    }
    let Some(l) = location else {
        println!("Location unknown.");
        return Ok(());
    };
    row(
        "Coordinates",
        format!("{:.6}, {:.6}", l.latitude, l.longitude),
    );
    row("As of", when(l.updated_at));
    row(
        "Map",
        format!(
            "https://www.google.com/maps?q={},{}",
            l.latitude, l.longitude
        ),
    );
    Ok(())
}

pub fn info(car: &Vehicle, caps: &Capabilities, m: &Maintenance, json: bool) -> Result<()> {
    if json {
        return print_json(&json!({ "vehicle": car, "capabilities": caps, "maintenance": m }));
    }
    let model = caps.model.as_deref().unwrap_or(&car.model);
    let year = caps.model_year.as_deref().unwrap_or("");
    let gearbox = caps.transmission.as_deref().unwrap_or("?");
    println!(
        "{}",
        clean(&format!("{} — {model} {year} ({gearbox})", car.name))
    );
    if let Some(km) = m.odometer_km {
        row("Odometer", format!("{km:.0} km"));
    }
    let features: Vec<&str> = [
        (caps.remote_start, "remote start"),
        (caps.horn_and_lights, "horn + lights"),
        (caps.lights_only, "lights only"),
        (caps.rear_defrost, "rear defrost"),
        (caps.steering_wheel_heat, "heated steering"),
        (caps.seat_ventilation, "ventilated seats"),
    ]
    .into_iter()
    .filter_map(|(has, name)| has.then_some(name))
    .collect();
    row("Remote features", features.join(", "));
    if let Some(end) = &caps.connectivity_ends {
        row("Connectivity until", end);
    }
    println!("\nService intervals:");
    for item in m.items.iter().filter(|i| i.enabled) {
        let km = item
            .interval
            .km
            .map(|k| format!("{k} km"))
            .unwrap_or_default();
        let months = match item.interval.months {
            Some(0) | None => String::new(),
            Some(n) => format!(" / {n} months"),
        };
        println!("  {:<22} {km}{months}", clean(&item.name));
    }
    Ok(())
}

pub fn command_sent(label: &str, id: &CommandId, check: &str, json: bool) -> Result<()> {
    if json {
        return print_json(&json!({ "command": label, "id": id, "status": "sent" }));
    }
    println!(
        "{}",
        clean(&format!("{label}: sent (id {id}). Check with `{check}`."))
    );
    Ok(())
}

/// A command whose result the server never reports (valet mode): say it was
/// sent and that the car's screen is the only confirmation, instead of
/// waiting for a result that never comes.
pub fn command_sent_unreported(label: &str, id: &CommandId, json: bool) -> Result<()> {
    if json {
        return print_json(&unreported_json(label, id));
    }
    println!("{}", clean(&unreported_text(label, id)));
    Ok(())
}

fn unreported_json(label: &str, id: &CommandId) -> serde_json::Value {
    json!({ "command": label, "id": id, "status": "sent", "result_reported": false })
}

fn unreported_text(label: &str, id: &CommandId) -> String {
    format!(
        "{label}: sent (id {id}). Hyundai's server doesn't report a result for this \
         command, so check the car's screen. Don't send it again to confirm."
    )
}

/// In `--json` mode, announce on stderr, as one JSON line, that a command was
/// accepted, before waiting for the car. The final result still goes to
/// stdout; this line means the id is known even if the wait is killed.
pub fn command_sent_event(label: &str, id: &CommandId) {
    eprintln!("{}", json!({ "event": "sent", "command": label, "id": id }));
}

pub fn command_done(
    label: &str,
    id: &CommandId,
    status: CommandStatus,
    took: Duration,
    check: &str,
    json: bool,
) -> Result<()> {
    if json {
        return print_json(
            &json!({ "command": label, "id": id, "status": status, "seconds": took.as_secs() }),
        );
    }
    let secs = took.as_secs();
    let message = match status {
        CommandStatus::Succeeded => format!("done ({secs} s)"),
        CommandStatus::Failed => {
            "the car refused it (is a door open or the engine already running?)".into()
        }
        CommandStatus::NoResponse => "the car didn't respond; it probably didn't act".into(),
        CommandStatus::Pending => {
            format!("still waiting after {secs} s; check later with `{check}`")
        }
        CommandStatus::Unknown => {
            format!("no matching result yet; check later with `{check}`")
        }
        other => format!("{other:?}; check later with `{check}`"),
    };
    println!("{}", clean(&format!("{label}: {message}")));
    Ok(())
}

/// The command was accepted, but its outcome is unknown because waiting
/// failed or was interrupted. The car may still act on it.
pub fn command_unconfirmed(
    label: &str,
    id: &CommandId,
    problem: &str,
    check: &str,
    json: bool,
) -> Result<()> {
    if json {
        print_json(&json!({ "command": label, "id": id, "status": "sent", "error": problem }))?;
    }
    let message = format!(
        "{label}: {problem}.\n\
         The command WAS sent (id {id}) and the car may still act on it. \
         Check its result with `{check}` before sending it again."
    );
    eprintln!("{}", clean(&message));
    Ok(())
}

pub fn command_status(status: CommandStatus, json: bool) -> Result<()> {
    if json {
        return print_json(&json!({ "status": status }));
    }
    println!("{status:?}");
    Ok(())
}

/// Today's date in India, where trip dates are reported.
pub fn india_today() -> jiff::civil::Date {
    let ist = jiff::tz::Offset::from_seconds(5 * 3600 + 30 * 60).expect("valid offset");
    Timestamp::now()
        .to_zoned_datetime(TimeZone::fixed(ist))
        .date()
}

fn clock(ts: Option<Timestamp>) -> String {
    ts.map_or_else(
        || "?".into(),
        |t| {
            t.to_zoned_datetime(TimeZone::system())
                .strftime("%H:%M")
                .to_string()
        },
    )
}

fn totals_line(t: &hkconnect::TripTotals) -> String {
    let km = t
        .distance_km
        .map_or_else(|| "?".into(), |d| format!("{d:.0} km"));
    let mins = t
        .drive_minutes
        .map_or_else(|| "?".into(), |m| format!("{m} min"));
    let max = t
        .max_speed_kmh
        .map_or_else(String::new, |s| format!(", top {s:.0} km/h"));
    format!("{km} in {mins}{max}")
}

pub fn trip_month(year: i16, month: i8, m: &TripMonth, json: bool) -> Result<()> {
    if json {
        return print_json(m);
    }
    println!("{year}-{month:02}: {}", totals_line(&m.totals));
    let mut days = m.days.clone();
    days.sort_by_key(|d| d.date);
    for d in &days {
        println!("  {}  {} trips", d.date, d.trips);
    }
    if !days.is_empty() {
        println!("\nDetails: hkc trips --day {}", days[days.len() - 1].date);
    }
    Ok(())
}

pub fn trip_day(day: &TripDay, json: bool) -> Result<()> {
    if json {
        return print_json(day);
    }
    println!("{}: {}", day.date, totals_line(&day.totals));
    for (i, t) in day.trips.iter().enumerate() {
        println!("  {}. {}-{}", i + 1, clock(t.started_at), clock(t.ended_at));
    }
    if !day.trips.is_empty() {
        println!(
            "\nRoute and driving events: hkc trips --day {} --trip 1",
            day.date
        );
    }
    Ok(())
}

/// A trip's detail as JSON: the library's [`TripDetail`] with the route
/// replaced by `route_points_total` unless `route` is set.
pub fn trip_detail_json(d: &TripDetail, route: bool) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(d)?;
    if !route && let Some(obj) = value.as_object_mut() {
        obj.remove("route");
        obj.insert("route_points_total".into(), d.route.len().into());
    }
    Ok(value)
}

pub fn trip_detail(n: usize, d: &TripDetail, json: bool, route: bool) -> Result<()> {
    if json {
        return print_json(&trip_detail_json(d, route)?);
    }
    println!(
        "Trip {n}: {}-{}, {}",
        clock(d.started_at),
        clock(d.ended_at),
        totals_line(&d.totals)
    );
    let count = |v: Option<u32>| v.map_or_else(|| "?".into(), |n| n.to_string());
    row("Harsh braking", count(d.hard_braking));
    row("Rapid accel.", count(d.rapid_acceleration));
    row("Speeding", count(d.speeding));
    row("Route points", d.route.len());
    if let (Some(first), Some(last)) = (d.route.first(), d.route.last()) {
        row(
            "From",
            format!("{:.5}, {:.5}", first.latitude, first.longitude),
        );
        row("To", format!("{:.5}, {:.5}", last.latitude, last.longitude));
    }
    println!("\nFull route: add --json --route");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_terminal_escapes_and_bidi_overrides() {
        let hostile = "Creta\u{1b}]0;pwned\u{7}\u{1b}[2J\r\u{9b}31m\u{202e}gnp.exe";
        let shown = clean(hostile);
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(!shown.contains('\u{202e}'), "{shown:?}");
        assert!(shown.starts_with("Creta\u{FFFD}]0;pwned"), "{shown:?}");
        assert_eq!(
            clean("line one\n\tline two — ok"),
            "line one\n\tline two — ok"
        );
    }

    fn detail_with_route(points: u32) -> TripDetail {
        let route: Vec<_> = (0..points)
            .map(|i| json!({ "at": null, "latitude": 12.97 + f64::from(i) * 1e-4, "longitude": 77.59 }))
            .collect();
        serde_json::from_value(json!({
            "started_at": "2026-11-17T02:30:03Z",
            "ended_at": "2026-11-17T02:46:34Z",
            "totals": { "drive_minutes": 15, "idle_minutes": 1, "distance_km": 13.0,
                        "average_speed_kmh": 59.0, "max_speed_kmh": 91.0 },
            "hard_braking": 1,
            "rapid_acceleration": 0,
            "speeding": 2,
            "route": route,
        }))
        .unwrap()
    }

    #[test]
    fn trip_json_leaves_out_the_route_unless_asked() {
        let d = detail_with_route(500);
        let lean = trip_detail_json(&d, false).unwrap();
        assert!(lean.get("route").is_none());
        assert_eq!(lean["route_points_total"], 500);
        assert_eq!(lean["hard_braking"], 1);
        assert_eq!(lean["speeding"], 2);
        assert_eq!(lean["totals"]["max_speed_kmh"], 91.0);

        let full = trip_detail_json(&d, true).unwrap();
        assert_eq!(full["route"].as_array().unwrap().len(), 500);
        assert!(full.get("route_points_total").is_none());
        assert!(lean.to_string().len() * 20 < full.to_string().len());
    }

    #[test]
    fn unreported_command_says_sent_and_to_check_the_car() {
        let id = CommandId::new("1fedaa50");
        let j = unreported_json("Valet mode", &id);
        assert_eq!(j["status"], "sent");
        assert_eq!(j["result_reported"], false);
        assert_eq!(j["id"], "1fedaa50");
        let text = unreported_text("Valet mode", &id);
        assert!(
            text.starts_with("Valet mode: sent (id 1fedaa50)."),
            "{text}"
        );
        assert!(text.contains("check the car's screen"), "{text}");
        assert!(text.contains("Don't send it again"), "{text}");
    }

    #[test]
    fn error_chain_includes_every_cause() {
        let inner = std::io::Error::other("connection refused");
        let outer = anyhow::Error::new(inner).context("network error");
        let e: &dyn std::error::Error = outer.as_ref();
        assert_eq!(error_chain(e), "network error: connection refused");
    }
}
