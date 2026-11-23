//! Attempt limiting for the OAuth endpoints.
//!
//! [`Limiter::default`] guards the sign-in password check. Two layers, both
//! charged *before* the (expensive) password hash is computed, so many
//! concurrent attempts can't all pass the check before any failure is
//! recorded:
//!
//! - Per client address (IPv6 grouped by /64): the first few wrong passwords
//!   are free, then each further one doubles a lockout, from 5 s up to
//!   15 min. A correct password clears the address.
//! - Global: at most [`GLOBAL_FAILURES_PER_HOUR`] wrong passwords per hour
//!   from all addresses together, so an attacker with many addresses still
//!   gets only a handful of guesses. The cost is that such an attacker can
//!   lock the owner out for up to an hour; restarting the server clears it.
//!
//! An attempt is counted as a failure when it is reserved
//! ([`Limiter::reserve`]), and given back if the password turns out to be
//! right ([`Limiter::success`]) or the attempt is abandoned before hashing
//! ([`Limiter::refund`]).
//!
//! [`Limiter::token`] is a per-address-only limiter (no global cap, so an
//! attacker cannot lock out the owner's token refreshes) for failed
//! `/token` requests. [`Window`] is a plain sliding-window counter, used to
//! cap client metadata fetches.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Wrong passwords allowed from one address before delays start.
const FREE_FAILURES: u32 = 3;
/// First delay after the free failures; doubles each time.
const BASE_DELAY: Duration = Duration::from_secs(5);
/// Longest per-address delay.
const MAX_DELAY: Duration = Duration::from_secs(15 * 60);
/// Wrong passwords per hour from everyone together.
pub const GLOBAL_FAILURES_PER_HOUR: usize = 30;
const HOUR: Duration = Duration::from_secs(3600);
/// Addresses remembered at most (oldest dropped first).
const MAX_TRACKED: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    blocked_until: Option<Instant>,
    last: Instant,
}

#[derive(Debug, Clone, Copy)]
struct Rules {
    free: u32,
    base: Duration,
    max: Duration,
    global_per_hour: Option<usize>,
}

/// Attempt limiter.
#[derive(Debug)]
pub struct Limiter {
    rules: Rules,
    per_address: HashMap<IpAddr, Entry>,
    global: VecDeque<Instant>,
}

impl Default for Limiter {
    /// The sign-in limiter.
    fn default() -> Self {
        Self::with_rules(Rules {
            free: FREE_FAILURES,
            base: BASE_DELAY,
            max: MAX_DELAY,
            global_per_hour: Some(GLOBAL_FAILURES_PER_HOUR),
        })
    }
}

/// A charged attempt: hand it back to [`Limiter::refund`] or
/// [`Limiter::success`] unless it really failed.
#[derive(Debug, Clone, Copy)]
pub struct Reservation {
    ip: IpAddr,
    at: Instant,
}

/// The key an address is limited under: IPv6 addresses by their /64, since
/// one host usually controls a whole /64.
pub fn key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        }
    }
}

impl Limiter {
    fn with_rules(rules: Rules) -> Self {
        Self {
            rules,
            per_address: HashMap::new(),
            global: VecDeque::new(),
        }
    }

    /// The limiter for failed `/token` requests: per address only (10 free,
    /// then 1 s doubling up to 5 min), no global cap.
    pub fn token() -> Self {
        Self::with_rules(Rules {
            free: 10,
            base: Duration::from_secs(1),
            max: Duration::from_secs(5 * 60),
            global_per_hour: None,
        })
    }

    /// `Err(wait)` if an attempt from `ip` must wait. Charges nothing.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        while self
            .global
            .front()
            .is_some_and(|t| now.duration_since(*t) >= HOUR)
        {
            self.global.pop_front();
        }
        let mut wait = Duration::ZERO;
        if let Some(cap) = self.rules.global_per_hour
            && self.global.len() >= cap
            && let Some(oldest) = self.global.front()
        {
            wait = HOUR.saturating_sub(now.duration_since(*oldest));
        }
        if let Some(until) = self.per_address.get(&key(ip)).and_then(|e| e.blocked_until)
            && until > now
        {
            wait = wait.max(until - now);
        }
        if wait.is_zero() { Ok(()) } else { Err(wait) }
    }

    /// Check and, if allowed, charge one failure in the same step, before
    /// the expensive work starts.
    pub fn reserve(&mut self, ip: IpAddr, now: Instant) -> Result<Reservation, Duration> {
        self.check(ip, now)?;
        self.failure(ip, now);
        Ok(Reservation { ip, at: now })
    }

    /// Give back an attempt abandoned before it was judged. A lockout it
    /// started is lifted only if the address is back within its free
    /// attempts.
    pub fn refund(&mut self, r: Reservation) {
        self.drop_global(r.at);
        if let Some(entry) = self.per_address.get_mut(&key(r.ip)) {
            entry.failures = entry.failures.saturating_sub(1);
            if entry.failures <= self.rules.free {
                entry.blocked_until = None;
            }
        }
    }

    /// The reserved attempt had the right password: give it back and clear
    /// the address.
    pub fn success(&mut self, r: Reservation) {
        self.drop_global(r.at);
        self.per_address.remove(&key(r.ip));
    }

    fn drop_global(&mut self, at: Instant) {
        if let Some(i) = self.global.iter().rposition(|t| *t == at) {
            self.global.remove(i);
        }
    }

    /// Record a failure from `ip`.
    pub fn failure(&mut self, ip: IpAddr, now: Instant) {
        if self.rules.global_per_hour.is_some() {
            self.global.push_back(now);
        }
        if self.per_address.len() >= MAX_TRACKED && !self.per_address.contains_key(&key(ip)) {
            self.evict_oldest();
        }
        let rules = self.rules;
        let entry = self.per_address.entry(key(ip)).or_insert(Entry {
            failures: 0,
            blocked_until: None,
            last: now,
        });
        entry.failures += 1;
        entry.last = now;
        if entry.failures > rules.free {
            let doublings = (entry.failures - rules.free - 1).min(16);
            let delay = rules.base.saturating_mul(1 << doublings).min(rules.max);
            entry.blocked_until = Some(now + delay);
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .per_address
            .iter()
            .min_by_key(|(_, e)| e.last)
            .map(|(k, _)| *k)
        {
            self.per_address.remove(&oldest);
        }
    }
}

/// A sliding-window event counter: at most `per_address_cap` events per
/// address and `global_cap` events overall within `window`.
#[derive(Debug)]
pub struct Window {
    span: Duration,
    per_address_cap: usize,
    global_cap: usize,
    per_address: HashMap<IpAddr, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

impl Window {
    /// A counter with these caps.
    pub fn new(window: Duration, per_address_cap: usize, global_cap: usize) -> Self {
        Self {
            span: window,
            per_address_cap,
            global_cap,
            per_address: HashMap::new(),
            global: VecDeque::new(),
        }
    }

    /// Count one event from `ip`, or return false if a cap is reached.
    pub fn take(&mut self, ip: IpAddr, now: Instant) -> bool {
        let span = self.span;
        let fresh = |t: &Instant| now.duration_since(*t) < span;
        self.global.retain(fresh);
        self.per_address.retain(|_, q| {
            q.retain(fresh);
            !q.is_empty()
        });
        let k = key(ip);
        let mine = self.per_address.get(&k).map_or(0, VecDeque::len);
        if self.global.len() >= self.global_cap || mine >= self.per_address_cap {
            return false;
        }
        self.global.push_back(now);
        self.per_address.entry(k).or_default().push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn backs_off_per_address_after_free_failures() {
        let mut l = Limiter::default();
        let now = Instant::now();
        let a = ip("203.0.113.7");
        for _ in 0..FREE_FAILURES {
            assert!(l.check(a, now).is_ok());
            l.failure(a, now);
        }
        assert!(l.check(a, now).is_ok());
        l.failure(a, now);
        assert_eq!(l.check(a, now), Err(Duration::from_secs(5)));
        // Another address is unaffected.
        assert!(l.check(ip("198.51.100.1"), now).is_ok());
        let later = now + Duration::from_secs(6);
        let r = l.reserve(a, later).unwrap();
        assert_eq!(l.check(a, later), Err(Duration::from_secs(10)));
        l.success(r);
        assert!(l.check(a, later).is_ok());
    }

    #[test]
    fn reservations_are_charged_before_the_work() {
        let mut l = Limiter::default();
        let now = Instant::now();
        let a = ip("203.0.113.7");
        // Many "concurrent" attempts: each reserves before any finishes.
        let granted: Vec<Reservation> = (0..100).filter_map(|_| l.reserve(a, now).ok()).collect();
        assert_eq!(granted.len(), FREE_FAILURES as usize + 1);
        // A refund of an abandoned attempt brings the address back under
        // its free attempts.
        let mut l = Limiter::default();
        let r: Vec<_> = (0..=FREE_FAILURES)
            .map(|_| l.reserve(a, now).unwrap())
            .collect();
        assert!(l.check(a, now).is_err());
        l.refund(r[0]);
        assert!(l.check(a, now).is_ok());
    }

    #[test]
    fn global_reservations_are_capped() {
        let mut l = Limiter::default();
        let now = Instant::now();
        let granted = (0..200)
            .filter(|i| {
                let a: IpAddr = format!("198.51.{}.{}", i / 250, i % 250).parse().unwrap();
                l.reserve(a, now).is_ok()
            })
            .count();
        assert_eq!(granted, GLOBAL_FAILURES_PER_HOUR);
        // A success refunds its global slot.
        let mut l = Limiter::default();
        let r = l.reserve(ip("192.0.2.1"), now).unwrap();
        l.success(r);
        assert!(l.global.is_empty());
    }

    #[test]
    fn token_limiter_has_no_global_cap() {
        let mut l = Limiter::token();
        let now = Instant::now();
        for i in 0..200u32 {
            let a: IpAddr = format!("198.51.100.{}", i % 200).parse().unwrap();
            l.failure(a, now);
        }
        assert!(l.check(ip("192.0.2.1"), now).is_ok());
        let a = ip("203.0.113.9");
        for _ in 0..10 {
            l.failure(a, now);
        }
        assert!(l.check(a, now).is_ok());
        l.failure(a, now);
        assert_eq!(l.check(a, now), Err(Duration::from_secs(1)));
    }

    #[test]
    fn delay_is_capped() {
        let mut l = Limiter::default();
        let now = Instant::now();
        let a = ip("203.0.113.7");
        // Fewer than the global limit, so only the per-address cap applies.
        for _ in 0..25 {
            l.failure(a, now);
        }
        assert_eq!(l.check(a, now), Err(MAX_DELAY));
    }

    #[test]
    fn global_limit_applies_across_addresses_and_expires() {
        let mut l = Limiter::default();
        let now = Instant::now();
        for i in 0..GLOBAL_FAILURES_PER_HOUR {
            let a: IpAddr = format!("198.51.100.{i}").parse().unwrap();
            l.failure(a, now);
        }
        let fresh = ip("192.0.2.200");
        assert!(l.check(fresh, now).is_err());
        assert!(l.check(fresh, now + HOUR).is_ok());
    }

    #[test]
    fn ipv6_is_grouped_by_64() {
        let mut l = Limiter::default();
        let now = Instant::now();
        for i in 0..=FREE_FAILURES {
            l.failure(format!("2001:db8:1:2::{i}").parse().unwrap(), now);
        }
        assert!(l.check(ip("2001:db8:1:2::ffff"), now).is_err());
        assert!(l.check(ip("2001:db8:1:3::1"), now).is_ok());
    }

    #[test]
    fn window_caps_per_address_and_globally() {
        let now = Instant::now();
        let mut w = Window::new(Duration::from_secs(60), 2, 3);
        let a = ip("203.0.113.1");
        assert!(w.take(a, now));
        assert!(w.take(a, now));
        assert!(!w.take(a, now));
        assert!(w.take(ip("203.0.113.2"), now));
        assert!(!w.take(ip("203.0.113.3"), now), "global cap");
        assert!(w.take(a, now + Duration::from_secs(61)));
    }
}
