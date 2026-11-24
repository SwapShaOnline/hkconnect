# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The `hkconnect`
library and the `hkc` CLI are versioned together.

## [Unreleased]

## [0.1.2] - 2026-11-24

### Added

- `hkc completions <shell>` prints a tab-completion script for bash, zsh,
  fish, elvish or PowerShell, and `hkc man` prints the manual page.
- An agent skill, `skills/hkconnect-car/SKILL.md`, that tells Claude Code
  and other agents when and how to use `hkc-mcp` (or `hkc`): report how old
  the data is, refresh sparingly, send commands once and check rather than
  resend.

### Changed

- Docs: remote start with climate (car confirmed in about 33 s) and stop
  (about 15 s, with the PIN control token) are now verified on a 2024
  Hyundai Creta.

## [0.1.1] - 2026-11-23

### Added

- Prebuilt `hkc` and `hkc-mcp` binaries for macOS, Linux and Windows on
  GitHub Releases, with shell and PowerShell installers, a Homebrew tap
  (`brew install SwapShaOnline/tap/hkc`) and `cargo binstall` support.
  Releases, including crates.io publishing, now run from a version tag.
- `hkc-mcp`: `get_trips` takes `with_events=true` with a date to include
  every trip's distance, top speed and harsh braking, rapid acceleration
  and speeding counts (plus day totals) in one call. The trips are fetched
  one at a time, at most 20 per call; the result says when it was cut short.
- `hkc trips --day D --trip N --json --route` includes the trip's GPS route.

### Changed

- `hkc-mcp`: `get_trips` with a date and trip now leaves out the GPS route
  unless `include_route=true`, returning `route_points_total` and a hint
  instead. A trip's totals and events drop from about 8 KB to about 0.6 KB.
- `hkc trips --day D --trip N --json` leaves out the route unless `--route`
  is given, and reports `route_points_total` instead.

### Fixed

- crates.io pages now link to the repository's new home,
  https://github.com/SwapShaOnline/hkconnect.
- The README describes `hkc-mcp`.

## [0.1.0] - 2026-11-23

Verified live on a 2024 Hyundai Creta: all reads, and lock (car confirmed in about 30 s). `hkc bench` measures API latency; results are in docs/performance.md.

Initial release.

### Added

- `hkconnect` library: async client for Hyundai Bluelink India and Kia
  Connect India (`Client`, `Config::india`, `Brand`, `Credentials`,
  `Session`).
  - Login through device registration, OAuth authorize, sign-in and token
    exchange; automatic token refresh; session persistence via
    `Client::session` and `Client::with_session`.
  - PIN to control-token exchange for commands that need it, cached until
    shortly before expiry.
  - Automatic recovery from an expired session (error 7501) and a rejected
    device registration (error 4002) by logging in or re-registering and
    retrying once.
  - Reads: vehicle list, cached status, forced status refresh, parking
    location, remote capabilities, odometer and service intervals, and trip
    history (monthly and daily summaries, trip routes and driving events).
  - Commands: lock, unlock, remote start with climate (`ClimateOptions`:
    temperature, duration, defrost, rear defrost, validated when built),
    stop, hazard lights and horn, valet mode, and sending a `Destination` to
    the car's navigation. Each returns a `CommandId`; `command_status` and
    `wait_for` report the car's result.
  - Typed errors for the server's known result codes (authentication, invalid
    PIN, duplicate request, unsupported command, car timeout, service
    unavailable, rate limited, no data). `Error::may_have_been_received`
    tells whether a failed command might still reach the car.
  - Lenient parsing: unknown fields are ignored, missing, null and
    string-typed values are tolerated, and list entries that cannot be read
    are skipped, so differences between models do not break parsing.
  - Data types serialize and deserialize symmetrically (snake_case field
    names), so they can be stored and read back; a stored `Trip` still works
    with `trip_detail`. Saved `Session`s carry a format version.
  - Newer CCS2-protocol vehicles are detected and rejected with a clear
    error.
- `hkc` command-line app: `vehicles`, `status [--refresh]`, `location`,
  `info`, `trips`, `lock`, `unlock`, `start`, `stop`, `hazard`, `valet`,
  `navigate`, `check`, `login`, `logout`.
  - Global options `--json`, `-c/--vehicle`, `--brand` and `-v/--verbose`.
  - Credentials from `HKC_*` environment variables or a `.env` file in the
    OS config directory.
  - Session saved with owner-only permissions; `hkc logout` deletes it.
  - Confirmation prompts for `unlock`, `start`, `hazard --horn`, `valet` and
    `navigate` (skip with `--yes`).
  - Commands wait for the car's result by default; `--no-wait` and
    `hkc check <id>` to look it up later.
- Test suite against a local fake server replaying scrubbed real responses.

### Fixed (pre-release review)

- `Client`: login, refresh and PIN requests are single-flight with
  remembered failure, so tasks queued behind a failed attempt get its error
  instead of repeating it (no burst of sign-ins with a wrong password). A
  PIN the server rejects is remembered for the life of the `Client` and
  never submitted again; HTTP 4xx answers to the PIN request, and HTTP
  401/403 right after a fresh login, count as `Error::InvalidPin`.
- `Error::may_have_been_received` is precise for failures before a command
  is sent (network errors while logging in or fetching the PIN token report
  `false`), and a command answered with an authentication-looking error
  that does not prove it was refused is reported as
  `Error::UnexpectedResponse` (`true`). The exact rule is documented.
- New `Error::InvalidVehicleId`: vehicle ids are validated before being put
  in a request path, so a hostile id cannot reach another endpoint.
- `Error::Http` now exposes its cause through `source()`; the URL (which
  contains the vehicle id) is dropped from transport errors.
- Leniency: token and control-token lifetimes sent as strings, numeric
  `resCode`s, `resMsg` of `null` or a string for status, maintenance,
  location and trips, a missing `tripInfo`, and non-finite numbers
  (`"NaN"`, `"inf"`, now read as missing).
- `jiff` and `url` are re-exported.
- `hkc`: saving the session is best effort (a warning, never a changed exit
  code); the `.env` file is parsed literally without `$VAR` substitution;
  server-provided text is stripped of control characters before printing;
  `--json` commands announce `{"event":"sent",...}` on stderr as soon as
  they are accepted; the `check` hint uses the vehicle id.

### Known limitations

- Reads are verified on a 2024 Hyundai Creta (petrol, automatic). Remote
  commands are tested against a fake server only and are experimental until
  verified on a real car.
- CCS2-protocol vehicles, EV charging features and regions other than India
  are not supported.
- API details deferred to 0.2 because changing them now would be breaking:
  `Brand`'s `FromStr` error is a plain `String`;
  `Error::UnsupportedVehicle` carries a `&'static str`; a `Session` is not
  bound to the brand or account it was created for (the `hkc` session file
  records both and checks them); and `hkc` keeps a single `session.json` per
  machine, so switching accounts or brands logs in again.

[Unreleased]: https://github.com/SwapShaOnline/hkconnect/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/SwapShaOnline/hkconnect/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/SwapShaOnline/hkconnect/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/SwapShaOnline/hkconnect/releases/tag/v0.1.0
