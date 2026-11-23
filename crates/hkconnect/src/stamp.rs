//! The `Stamp` header: `base64(CFB XOR "{app_id}:{unix_seconds}")`.
//!
//! The server uses it to check that requests come from a genuine app build
//! and were made recently.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Build a stamp for the given time. Pure, so it can be tested exactly.
pub(crate) fn stamp(app_id: &str, cfb: &[u8], unix_seconds: i64) -> String {
    let raw = format!("{app_id}:{unix_seconds}");
    // `zip` stops at the shorter input, matching the official apps.
    let xored: Vec<u8> = cfb.iter().zip(raw.bytes()).map(|(k, b)| k ^ b).collect();
    STANDARD.encode(xored)
}

pub(crate) fn stamp_now(app_id: &str, cfb: &[u8]) -> String {
    stamp(app_id, cfb, jiff::Timestamp::now().as_second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Brand, Config};

    #[test]
    fn matches_reference_implementation() {
        // Expected value produced by the Python reference implementation.
        let cfg = Config::india(Brand::Hyundai);
        assert_eq!(
            stamp(cfg.app_id, &cfg.cfb, 1_700_000_000),
            "cTpaccuJNUJLchQJVItk+MacmovIEpi4IFlOub+vS0WDfPBIn3rSYiqWOyDGh9A="
        );
    }
}
