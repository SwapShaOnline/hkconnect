//! Async client for Hyundai Bluelink India and Kia Connect India.
//!
//! `hkconnect` talks to the same undocumented servers as the official India
//! apps, using your own account. It is unofficial and not affiliated with
//! Hyundai or Kia; the API can change without notice.
//!
//! # Features
//!
//! - Login with automatic token refresh, and a [`Session`] you can persist to
//!   skip the slow login on the next run ([`Client::with_session`]).
//! - Reads: [`Client::vehicles`], cached [`Client::status`] and
//!   car-waking [`Client::refresh_status`], [`Client::location`],
//!   [`Client::maintenance`], [`Client::capabilities`], and trip history
//!   ([`Client::trips_in_month`], [`Client::trips_on_day`],
//!   [`Client::trip_detail`]).
//! - Remote commands: lock/unlock, remote start with climate, stop, hazard
//!   lights/horn, valet mode and sending a destination. Commands return a
//!   [`CommandId`] as soon as the server accepts them; use
//!   [`Client::wait_for`] to learn whether the car actually acted.
//! - Errors mapped from the server's result codes into [`Error`].
//! - Lenient parsing: fields a car may omit are `Option`, numbers sent as
//!   strings are accepted, and a list entry that cannot be read is skipped
//!   instead of failing the whole call. Timestamps (sent in Indian Standard
//!   Time) are converted to UTC [`jiff::Timestamp`]s.
//! - Data types implement `Serialize` and `Deserialize` with the same
//!   `snake_case` field names both ways, so they can be stored or sent as
//!   JSON and read back; a stored [`Trip`] still works with
//!   [`Client::trip_detail`].
//!
//! # Status
//!
//! Reads have been verified against a real 2024 Hyundai Creta. Remote
//! commands are experimental: they follow the reference implementation and
//! are tested against a fake server, but are not yet verified on a real car.
//! Kia uses the same protocol on a different host and is untested. Newer cars
//! that use the "CCS2" protocol are not supported: [`Client::status`],
//! [`Client::refresh_status`] and the remote commands return
//! [`Error::UnsupportedVehicle`] for them.
//!
//! # Example
//!
//! Log in, pick the first car and read its cached status. The `hkc`
//! command-line crate in the same repository is a complete program built on
//! this library.
//!
//! ```no_run
//! # async fn demo() -> hkconnect::Result<()> {
//! use hkconnect::{Brand, Client, Config, Credentials};
//!
//! let client = Client::new(
//!     Config::india(Brand::Hyundai),
//!     Credentials::new("you@example.com", "password").with_pin("1234"),
//! )?;
//! let car = client.vehicles().await?.remove(0);
//! let status = client.status(&car).await?;
//! println!("locked: {:?}", status.locked);
//! # Ok(()) }
//! ```

mod auth;
mod client;
mod config;
mod de;
mod envelope;
mod error;
mod model;
mod session;
mod stamp;
mod temperature;
mod trips;

/// The `jiff` crate, re-exported: timestamps and dates in this crate's API
/// ([`VehicleStatus::updated_at`], [`Client::trips_on_day`], ...) are `jiff`
/// types, so you can name them without adding a matching `jiff` dependency.
pub use jiff;
/// The `url` crate, re-exported: [`Config::with_base_url`] takes a
/// [`url::Url`], so you can build one without adding a matching `url`
/// dependency.
pub use url;

pub use client::Client;
pub use config::{Brand, Config};
pub use error::{Error, HttpError, Result};
pub use model::{
    Battery12V, Capabilities, ClimateOptions, CommandId, CommandStatus, Destination, Distance,
    DistanceUnit, Doors, Location, Maintenance, MaintenanceItem, Powertrain, ServiceInterval,
    Vehicle, VehicleStatus,
};
pub use session::{Credentials, Session};
pub use temperature::Celsius;
pub use trips::{
    Coordinates, RoutePoint, Trip, TripDay, TripDayCount, TripDetail, TripMonth, TripTotals,
};
