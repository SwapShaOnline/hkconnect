//! Sign-in attempt limiting.
//!
//! Two layers, checked before the (expensive) password hash is computed:
//!
//! - Per client address (IPv6 grouped by /64): the first few wrong passwords
//!   are free, then each further one doubles a lockout, from 5 s up to
//!   15 min. A correct password clears the address.
//! - Global: at most [`GLOBAL_FAILURES_PER_HOUR`] wrong passwords per hour
//!   from all addresses together, so an attacker with many addresses still
//!   gets only a handful of guesses. The cost is that such an attacker can
//!   lock the owner out for up to an hour; restarting the server clears it.

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

/// Sign-in attempt limiter.
#[derive(Debug, Default)]
pub struct Limiter {
    per_address: HashMap<IpAddr, Entry>,
    global: VecDeque<Instant>,
}

/// The key an address is limited under: IPv6 addresses by their /64, since
/// one host usually controls a whole /64.
fn key(ip: IpAddr) -> IpAddr {
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
    /// `Err(wait)` if an attempt from `ip` must wait.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        while self
            .global
            .front()
            .is_some_and(|t| now.duration_since(*t) >= HOUR)
        {
            self.global.pop_front();
        }
        let mut wait = Duration::ZERO;
        if self.global.len() >= GLOBAL_FAILURES_PER_HOUR
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

    /// Record a wrong password from `ip`.
    pub fn failure(&mut self, ip: IpAddr, now: Instant) {
        self.global.push_back(now);
        if self.per_address.len() >= MAX_TRACKED && !self.per_address.contains_key(&key(ip)) {
            self.evict_oldest();
        }
        let entry = self.per_address.entry(key(ip)).or_insert(Entry {
            failures: 0,
            blocked_until: None,
            last: now,
        });
        entry.failures += 1;
        entry.last = now;
        if entry.failures > FREE_FAILURES {
            let doublings = (entry.failures - FREE_FAILURES - 1).min(16);
            let delay = BASE_DELAY.saturating_mul(1 << doublings).min(MAX_DELAY);
            entry.blocked_until = Some(now + delay);
        }
    }

    /// Record a correct password from `ip`.
    pub fn success(&mut self, ip: IpAddr) {
        self.per_address.remove(&key(ip));
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
        assert!(l.check(a, later).is_ok());
        l.failure(a, later);
        assert_eq!(l.check(a, later), Err(Duration::from_secs(10)));
        l.success(a);
        assert!(l.check(a, later).is_ok());
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
}
