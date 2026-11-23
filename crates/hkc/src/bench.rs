//! `hkc bench`: measure how long each read-only API call takes.
//!
//! Only endpoints that return stored data are used, so the car is never
//! woken and nothing on it changes. Calls are spaced out to stay well clear
//! of the account's rate limit.

use std::time::{Duration, Instant};

use anyhow::Result;
use hkconnect::{Client, Vehicle};
use serde::Serialize;

/// Timing summary for one operation, in milliseconds.
#[derive(Debug, Serialize)]
pub struct Stats {
    name: &'static str,
    runs: usize,
    min_ms: u64,
    median_ms: u64,
    p95_ms: u64,
    max_ms: u64,
}

impl Stats {
    fn from_samples(name: &'static str, mut samples: Vec<Duration>) -> Self {
        samples.sort();
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let at = |q: f64| {
            // Nearest-rank percentile.
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "small sample counts"
            )]
            let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
            ms(samples[rank - 1])
        };
        Self {
            name,
            runs: samples.len(),
            min_ms: ms(samples[0]),
            median_ms: at(0.5),
            p95_ms: at(0.95),
            max_ms: ms(samples[samples.len() - 1]),
        }
    }
}

/// Time a fresh login once, then each read-only call `runs` times.
pub async fn run(client: &Client, car: &Vehicle, runs: usize, gap: Duration) -> Result<Vec<Stats>> {
    let runs = runs.max(1);
    let mut results = Vec::new();

    let started = Instant::now();
    client.login().await?;
    results.push(Stats::from_samples(
        "login (fresh)",
        vec![started.elapsed()],
    ));

    let today = crate::output::india_today();
    macro_rules! measure {
        ($name:literal, $call:expr) => {{
            let mut samples = Vec::with_capacity(runs);
            for _ in 0..runs {
                tokio::time::sleep(gap).await;
                let started = Instant::now();
                $call.await?;
                samples.push(started.elapsed());
            }
            results.push(Stats::from_samples($name, samples));
        }};
    }
    measure!("vehicles", client.vehicles());
    measure!("status (cached)", client.status(car));
    measure!("location", client.location(car));
    measure!("info: capabilities", client.capabilities(car));
    measure!("info: maintenance", client.maintenance(car));
    measure!(
        "trips: month",
        client.trips_in_month(car, today.year(), today.month())
    );
    measure!("trips: day", client.trips_on_day(car, today));
    Ok(results)
}

/// Print a table, or JSON.
pub fn print(results: &[Stats], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(results)?);
        return Ok(());
    }
    println!(
        "{:<22} {:>4} {:>8} {:>8} {:>8} {:>8}",
        "operation", "runs", "min ms", "median", "p95", "max"
    );
    for s in results {
        println!(
            "{:<22} {:>4} {:>8} {:>8} {:>8} {:>8}",
            s.name, s.runs, s.min_ms, s.median_ms, s.p95_ms, s.max_ms
        );
    }
    println!("\nRead-only calls only; the car was not woken.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples = (1..=20).map(Duration::from_millis).collect();
        let s = Stats::from_samples("x", samples);
        assert_eq!((s.min_ms, s.median_ms, s.p95_ms, s.max_ms), (1, 10, 19, 20));
        let one = Stats::from_samples("y", vec![Duration::from_millis(7)]);
        assert_eq!((one.median_ms, one.p95_ms), (7, 7));
    }
}
