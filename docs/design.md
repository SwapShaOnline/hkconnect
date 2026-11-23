# hkconnect — design

A Rust client and CLI for Hyundai Bluelink India and Kia Connect India:
read vehicle status and location, and send remote commands (lock/unlock,
remote start with climate, hazard lights/horn). Built to become an MCP
server later, so Claude (on desktop or phone) can control the car without a
laptop running.

## Why

- The owner wants to tell Claude "start the AC", "is the car locked?", and get
  a sensible answer, from anywhere.
- The existing Python library (`hyundai_kia_connect_api`) works but covers 9
  regions, is synchronous, and logs routine responses at ERROR. India only
  needs ~12 endpoints.
- Rust was chosen deliberately as a learning project. Performance is not the
  reason (the car's modem adds 10–40 s to every command); a tiny, single
  binary that is easy to host is.

## Goals

- Typed, async, well-tested core library (`hkconnect`) usable from a CLI, an
  MCP server, or anything else.
- Hyundai India and Kia India from day one (same protocol, different
  hosts/keys). Any model, not just the Creta.
- Session reuse: log in once, persist tokens, refresh automatically. Reads
  should take ~0.5 s, not ~4 s.
- Clear errors mapped from the server's `resCode`s (rate limit, timeout,
  bad PIN, unsupported command, …).
- Safety for commands that start the engine or unlock the car: explicit
  confirmation in the CLI; asynchronous result tracking so "sent" is never
  confused with "done".

## Non-goals (for now)

- Other regions (EU/US/CA…). The design keeps the door open (a `Region`
  config struct) but we don't build them.
- EV charging features and schedules (no EV to verify against).
- Newer CCS2 cars (different status format; no car to verify against).
- Hosting the MCP server (the server itself is built: `crates/hkc-mcp`, see
  [mcp-design.md](mcp-design.md) and [mcp.md](mcp.md)).

## Anti-goals

- Hammering the car: no automatic "force refresh" polling. Each force refresh
  wakes the car and drains the 12V battery.
- Logging secrets. Tokens, passwords, PINs, and cookies never reach logs.
- Pretending a command succeeded because the server accepted it.

## Approaches considered

| Approach | Trade-off | Decision |
|---|---|---|
| Wrap the Python library | Fastest; inherits sync design and noise | Rejected |
| Own Python client | Easiest port | Rejected: owner chose Rust to learn it |
| **Own Rust client, India-only** | Port effort; we own fixes when Hyundai changes the API | **Chosen** |
| TypeScript on serverless | Free hosting, mature MCP SDK | Rejected for now |

## Protocol (observed 2026-11-23 against a 2024 Creta)

Base: `https://prd.in-ccapi.{hyundai|kia}.connected-car.io:8080`

Login (≈0.3 s with a reused connection; see [performance.md](performance.md)):
1. `POST /api/v1/spa/notifications/register` → `deviceId`. Needs a `Stamp`
   header: base64(CFB XOR "{APP_ID}:{unix_seconds}").
2. `GET /api/v1/user/oauth2/authorize?...` → session cookie.
3. `POST /api/v1/user/signin {email,password}` (with cookie) →
   `redirectUrl` containing `code`.
4. `POST /api/v1/user/oauth2/token` (Basic auth, form body,
   `grant_type=authorization_code`) → access token (1 h), refresh token.
5. Refresh: same endpoint, `grant_type=refresh_token`, with `Stamp`.
6. Control token (PIN): `PUT /api/v1/user/pin?token= {deviceId,pin}` →
   `controlToken`, `expiresTime`. Used by some commands.

Reads (≈0.07–0.35 s each, see performance.md): `/spa/vehicles`, `/vehicles/{id}/status/latest`
(cached), `/vehicles/{id}/status` (force refresh — wakes the car),
`/vehicles/{id}/location/park`, `/vehicles/{id}/profile` (capabilities),
`/vehicles/{id}/setting/alert/maintenance` (odometer + service intervals),
`/notifications/{id}/records` (command results).

Commands for non-CCS2 vehicles (`ccuCCS2ProtocolSupport == 0`, like the
Creta) return a `msgId`; the result appears later in
`/notifications/{id}/records` as `success | fail | non-response | null`.
- Lock/unlock: `POST v1 /vehicles/{id}/control/door {action: close|open, deviceId}`
- Remote start + climate: `POST v1 /vehicles/{id}/control/engine
  {action:"start", hvacType:1, options:{defrost, heating1, igniOnDuration},
  tempCode:"10H", unit:"C"}`. Temperature 14.0–29.5 °C in 0.5 steps, encoded
  as the step index in hex + "H".
- Stop: `POST v2 /vehicles/{id}/control/engine {action:"stop"}` with the
  control token.
- Valet mode: `POST v2 /vehicles/{id}/control/valet {action: activate|deactivate}`
  with the control token.
- Send destination: `POST v2 /vehicles/{id}/location/routes {deviceID,
  poiInfoList: [{name, addr, coord{lat,lon,alt,type}, src:"HERE", …}]}` with
  the control token.
- Hazard / horn: `POST v2 /vehicles/{id}/ccs2/control/{light|hornlight}`
  with the control token. Unverified on non-CCS2 cars → marked experimental.

Trip history (verified on the Creta): `POST v1 /vehicles/{id}/tripinfo` with
`{tripPeriodType:0, setTripMonth:"YYYYMM"}` (month totals, days with trips)
or `{tripPeriodType:1, setTripDay:"YYYYMMDD"}` (the day's trips, newest
first); `POST v1 /vehicles/{id}/tripinfo/detail` echoing a trip's raw
`tripStartTime`/`tripEndTime`/`serviceTID` returns its GPS route and harsh
braking / rapid acceleration / speeding counts. The reference implementation
skips the detail call for Hyundai non-EVs; it works on the Creta.

The full endpoint inventory and status is in [api-coverage.md](api-coverage.md).

Envelope: `{retCode: "S"|"F", resCode, resMsg, msgId}`. Known failure codes:
4002 invalid device id (re-register and retry once), 4004 duplicate,
4005 unsupported, 4081/9999 timeout, 5031 service unavailable,
5091 rate limited, 5921 no data, 7501 auth.

## Architecture

```
crates/
  hkconnect/        library
    config.rs       Brand → hosts, client ids, keys (Region-shaped)
    stamp.rs        Stamp header generation (pure, unit-tested)
    auth.rs         login, refresh, device registration, control token
    session.rs      Session {tokens, device id, expiry}; serde for persistence
    envelope.rs     response envelope + resCode → Error mapping
    model.rs        Vehicle, Status, Location, Capabilities, commands, …
    trips.rs        trip history: month, day, per-trip route and events
    temperature.rs  Celsius newtype with validation + hex encoding
    client.rs       Client: high-level async API
    error.rs        thiserror enum
  hkc/              CLI binary (clap)
  hkc-mcp/          MCP server for Claude (rmcp; stdio + OAuth HTTP)
tests/fixtures/     scrubbed real responses
```

Key choices (builder's calls, made here):
- `reqwest` (rustls, cookies, gzip, json) on `tokio`; base URL injectable so
  tests run against `wiremock` with recorded, scrubbed fixtures.
- Lenient parsing: responses are read through private wire structs whose
  fields are `Option` with `#[serde(default)]` and tolerant deserializers
  (numbers as strings, nulls, wrong shapes become `None`); list entries that
  don't parse are skipped; unknown fields are ignored. Hyundai's JSON is
  undocumented and varies by model, so parsing must never fail on a field
  we don't use. The public types derive symmetric serde (snake_case), so
  they can be persisted and read back.
- `jiff` for timestamps (server sends `YYYYMMDDHHMMSS` in IST).
- `secrecy::SecretString` for password, PIN and tokens (redacted `Debug`).
- CLI: credentials from env (`HKC_USERNAME`, `HKC_PASSWORD`, `HKC_PIN`,
  optional `.env` in the OS config dir only; never the current directory);
  session saved in the OS data dir with 0600 perms;
  `--json` output on every read; `unlock`, `start`, `hazard --horn`,
  `valet` and `navigate` require `--yes` or an interactive confirmation
  (`lock`, `stop` and `hazard` without `--horn` do not ask); commands wait for
  the car's result by default (`--no-wait` to skip). The `.env` file is
  parsed literally (no `$VAR` expansion); saving the session is best effort
  and never changes a command's exit code.
- Concurrency: one login/refresh and one PIN request at a time; tasks that
  waited on a failed attempt get its error instead of repeating it. A PIN
  the server rejects is never resent by that `Client` (wrong PINs can block
  remote control).
- Vehicle ids are checked (ASCII letters, digits, `-`, `_`, `.`; no `..`)
  before they are put in a request path.
- Quality gates: `cargo fmt`, `cargo clippy -D warnings` (pedantic, with
  justified allows), tests, CI workflow.

## Open questions / to verify live

- Does stop-engine need the control token on India non-CCS2? (Library says
  yes; start says no.) Verify when the owner approves a live test.
- Do hazard/horn work on non-CCS2 India cars?
- Exact meaning of some status fields (e.g. `airTemp.value: "00H"` while off).
