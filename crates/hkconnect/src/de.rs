//! Lenient deserializers for Hyundai's loosely typed JSON.
//!
//! The same flag can arrive as `true`, `1`, `"1"`, or `"Y"` depending on the
//! endpoint and car model, so we accept all of them.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{Offset, TimeZone};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// India Standard Time (UTC+05:30, no daylight saving). The API reports
/// local times without an offset.
fn ist() -> TimeZone {
    TimeZone::fixed(Offset::from_seconds(5 * 3600 + 30 * 60).expect("valid offset"))
}

pub(crate) fn truthy(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "y" | "yes" | "true" => Some(true),
            "0" | "n" | "no" | "false" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn flexible_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.as_ref().and_then(truthy))
}

pub(crate) fn flexible_bool_or_false<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(flexible_bool(d)?.unwrap_or(false))
}

/// A number sent as a number or a string; `None` if absent, unparsable, or
/// not finite (`"NaN"` and `"inf"` parse as floats but cannot be written back
/// as JSON, so they would break serde round trips).
pub(crate) fn flexible_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let n = match Option::<Value>::deserialize(d)? {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    Ok(n.filter(|f| f.is_finite()))
}

/// Decode a payload that should be a JSON object. Anything else (`null`, a
/// string, a list) is read as `{}`, so a reply with nothing useful in it
/// becomes an empty value instead of an error. Only for types whose fields
/// all have defaults.
pub(crate) fn object_or_empty<T: DeserializeOwned>(value: Value) -> serde_json::Result<T> {
    let value = if value.is_object() {
        value
    } else {
        tracing::debug!(
            kind = std::any::type_name::<T>(),
            "expected an object; treating it as empty"
        );
        Value::Object(serde_json::Map::new())
    };
    serde_json::from_value(value)
}

/// An integer sent as a number or a string; `None` if absent, fractional,
/// or out of range for `T`.
pub(crate) fn flexible_int<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<i64>,
{
    Ok(flexible_f64(d)?.and_then(|f| {
        #[expect(clippy::cast_possible_truncation, reason = "fract() == 0 checked")]
        let whole = (f.fract() == 0.0).then_some(f as i64)?;
        T::try_from(whole).ok()
    }))
}

/// A string sent as a string or a number (`"2024"` or `2024`); `None` if
/// absent, null, or any other type.
pub(crate) fn flexible_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// An optional nested value that is `None` (logged at debug level) when it
/// does not have the expected shape, instead of failing the whole response.
pub(crate) fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(Option::<Value>::deserialize(d)?.and_then(parse_or_skip))
}

/// A list whose entries are parsed one by one: an entry that does not parse
/// is skipped (and logged at debug level) rather than failing the whole
/// list. A missing, null or non-array value is an empty list.
pub(crate) fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Array(items)) => items.into_iter().filter_map(parse_or_skip).collect(),
        None => Vec::new(),
        Some(_) => {
            tracing::debug!(
                kind = std::any::type_name::<T>(),
                "expected a list; treating it as empty"
            );
            Vec::new()
        }
    })
}

fn parse_or_skip<T: DeserializeOwned>(value: Value) -> Option<T> {
    serde_json::from_value(value)
        .inspect_err(|e| {
            tracing::debug!(
                kind = std::any::type_name::<T>(),
                error = %e,
                "skipping a value that does not parse"
            );
        })
        .ok()
}

/// Parse `"20261123161134"` (IST) into a UTC timestamp.
pub(crate) fn parse_ist(raw: &str) -> Option<Timestamp> {
    let civil = DateTime::strptime("%Y%m%d%H%M%S", raw).ok()?;
    ist().to_timestamp(civil).ok()
}

/// Parse `"20261117023345"` given in UTC.
pub(crate) fn parse_utc(raw: &str) -> Option<Timestamp> {
    let civil = DateTime::strptime("%Y%m%d%H%M%S", raw).ok()?;
    TimeZone::UTC.to_timestamp(civil).ok()
}

pub(crate) fn ist_timestamp<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Timestamp>, D::Error> {
    Ok(flexible_string(d)?.as_deref().and_then(parse_ist))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthy_accepts_every_shape_the_api_uses() {
        for v in [json!(true), json!(1), json!("1"), json!("Y")] {
            assert_eq!(truthy(&v), Some(true), "{v}");
        }
        for v in [json!(false), json!(0), json!("0"), json!("N")] {
            assert_eq!(truthy(&v), Some(false), "{v}");
        }
        assert_eq!(truthy(&json!("maybe")), None);
    }

    #[test]
    fn lenient_lists_skip_bad_entries() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Item {
            n: u8,
        }
        #[derive(Deserialize)]
        struct List {
            #[serde(default, deserialize_with = "lenient_vec")]
            items: Vec<Item>,
        }
        let parse = |v: Value| serde_json::from_value::<List>(v).unwrap().items;
        let items = parse(json!({ "items": [{ "n": 1 }, "junk", { "n": null }, { "n": 2 }] }));
        assert_eq!(items, [Item { n: 1 }, Item { n: 2 }]);
        assert_eq!(parse(json!({ "items": null })).len(), 0);
        assert_eq!(parse(json!({ "items": { "n": 1 } })).len(), 0);
        assert_eq!(parse(json!({})).len(), 0);
    }

    #[test]
    fn flexible_string_accepts_numbers() {
        #[derive(Deserialize)]
        struct S {
            #[serde(default, deserialize_with = "flexible_string")]
            s: Option<String>,
        }
        let parse = |v: Value| serde_json::from_value::<S>(v).unwrap().s;
        assert_eq!(parse(json!({ "s": 12 })).as_deref(), Some("12"));
        assert_eq!(parse(json!({ "s": "x" })).as_deref(), Some("x"));
        assert_eq!(parse(json!({ "s": null })), None);
        assert_eq!(parse(json!({ "s": [1] })), None);
        assert_eq!(parse(json!({})), None);
    }

    #[test]
    fn flexible_numbers_reject_non_finite_values() {
        #[derive(Deserialize)]
        struct N {
            #[serde(default, deserialize_with = "flexible_f64")]
            f: Option<f64>,
            #[serde(default, deserialize_with = "flexible_int")]
            i: Option<i64>,
        }
        let parse = |v: Value| {
            let n = serde_json::from_value::<N>(v).unwrap();
            (n.f, n.i)
        };
        for bad in ["NaN", "nan", "inf", "-infinity", "1e400"] {
            assert_eq!(parse(json!({ "f": bad, "i": bad })), (None, None), "{bad}");
        }
        assert_eq!(
            parse(json!({ "f": " 2.5 ", "i": "3600" })),
            (Some(2.5), Some(3600))
        );
        assert_eq!(parse(json!({ "f": 1, "i": 2.5 })), (Some(1.0), None));
    }

    #[test]
    fn non_objects_decode_as_empty() {
        #[derive(Debug, Default, Deserialize, PartialEq)]
        struct S {
            #[serde(default)]
            a: Option<u8>,
        }
        for v in [json!(null), json!("x"), json!([1]), json!(3)] {
            assert_eq!(object_or_empty::<S>(v).unwrap(), S::default());
        }
        assert_eq!(
            object_or_empty::<S>(json!({ "a": 1 })).unwrap(),
            S { a: Some(1) }
        );
    }

    #[test]
    fn parses_ist_into_utc() {
        let ts = parse_ist("20261123161134").unwrap();
        assert_eq!(ts.to_string(), "2026-11-23T10:41:34Z");
        assert_eq!(parse_ist("not a time"), None);
    }
}
