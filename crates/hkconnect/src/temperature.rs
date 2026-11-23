//! Cabin temperature, as the climate API understands it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Error;

/// A climate set-point in °C: 14.0–29.5 in 0.5 steps.
///
/// The API encodes it as the step index in hex followed by `H`
/// (14.0 °C → `"00H"`, 22.0 °C → `"10H"`). Storing the index means an invalid
/// temperature cannot exist once constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Celsius {
    half_steps_above_min: u8,
}

impl Celsius {
    /// The lowest set-point the API accepts, in °C.
    pub const MIN: f32 = 14.0;
    /// The highest set-point the API accepts, in °C.
    pub const MAX: f32 = 29.5;
    const STEPS: u8 = 32; // (29.5 - 14.0) / 0.5 + 1
    /// 22 °C, the default for remote start.
    pub(crate) const DEFAULT: Self = Self {
        half_steps_above_min: 16,
    };

    /// Validate a temperature in °C.
    ///
    /// Fails with [`Error::InvalidTemperature`] unless it is within
    /// [`MIN`](Self::MIN)..=[`MAX`](Self::MAX) and a multiple of 0.5.
    pub fn new(degrees: f32) -> Result<Self, Error> {
        let steps = (degrees - Self::MIN) * 2.0;
        let rounded = steps.round();
        let valid = degrees.is_finite()
            && (Self::MIN..=Self::MAX).contains(&degrees)
            && (steps - rounded).abs() < f32::EPSILON;
        if !valid {
            return Err(Error::InvalidTemperature(degrees));
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "range checked above: 0..=31"
        )]
        Ok(Self {
            half_steps_above_min: rounded as u8,
        })
    }

    /// The temperature in °C.
    #[must_use]
    pub fn degrees(self) -> f32 {
        Self::MIN + f32::from(self.half_steps_above_min) / 2.0
    }

    /// The wire format, e.g. `"10H"`.
    #[must_use]
    pub fn to_code(self) -> String {
        format!("{:02X}H", self.half_steps_above_min)
    }

    /// Parse the wire format: exactly two hex digits (either case) and `H`.
    /// Returns `None` for anything else or for codes outside the range.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        let digits = code.strip_suffix('H')?;
        // `from_str_radix` alone would also accept a sign, as in "+0AH".
        if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let index = u8::from_str_radix(digits, 16).ok()?;
        (index < Self::STEPS).then_some(Self {
            half_steps_above_min: index,
        })
    }
}

impl fmt::Display for Celsius {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} °C", self.degrees())
    }
}

/// Serializes as the number of degrees, e.g. `22.5`.
impl Serialize for Celsius {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f32(self.degrees())
    }
}

/// Deserializes from a number of degrees, validated like [`Celsius::new`].
impl<'de> Deserialize<'de> for Celsius {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(f32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Deserializes the API's `{"value": "10H", ...}` object; `None` for any
/// other shape or an unknown code.
pub(crate) fn from_air_temp<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Celsius>, D::Error> {
    let parsed = Option::<serde_json::Value>::deserialize(d)?;
    Ok(parsed
        .as_ref()
        .and_then(|t| t.get("value"))
        .and_then(serde_json::Value::as_str)
        .and_then(Celsius::from_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_the_official_app() {
        assert_eq!(Celsius::new(14.0).unwrap().to_code(), "00H");
        assert_eq!(Celsius::new(16.5).unwrap().to_code(), "05H");
        assert_eq!(Celsius::new(22.0).unwrap().to_code(), "10H");
        assert_eq!(Celsius::new(29.5).unwrap().to_code(), "1FH");
    }

    #[test]
    fn round_trips_every_valid_value() {
        for i in 0..Celsius::STEPS {
            let t = Celsius::new(Celsius::MIN + f32::from(i) / 2.0).unwrap();
            assert_eq!(Celsius::from_code(&t.to_code()), Some(t));
        }
    }

    #[test]
    fn default_is_22() {
        assert_eq!(Celsius::DEFAULT, Celsius::new(22.0).unwrap());
    }

    #[test]
    fn serde_uses_degrees_and_validates() {
        let t = Celsius::new(22.5).unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), "22.5");
        assert_eq!(serde_json::from_str::<Celsius>("22.5").unwrap(), t);
        assert!(serde_json::from_str::<Celsius>("40").is_err());
    }

    #[test]
    fn rejects_invalid_values() {
        for bad in [13.5, 30.0, 22.3, f32::NAN, f32::INFINITY] {
            assert!(Celsius::new(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(Celsius::from_code("20H"), None);
        assert_eq!(Celsius::from_code("garbage"), None);
        for bad in ["+0AH", "-1H", "0AAH", "AH", "0A", "0aH ", " 0AH", "0Ah"] {
            assert_eq!(Celsius::from_code(bad), None, "{bad:?}");
        }
        assert_eq!(Celsius::from_code("0aH"), Celsius::from_code("0AH"));
        assert!(Celsius::from_code("0aH").is_some());
    }
}
