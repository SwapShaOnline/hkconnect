# hkc-mcp — design

An MCP server that lets Claude (web, desktop, and the mobile apps) read and
control a Hyundai/Kia (India) car through the `hkconnect` library.

Status: design, not yet built. Hosting location is an open decision (below).

## Why

The owner wants to say "start the AC", "is the car locked?", "where did the
car go today?" to Claude from a phone, without a laptop running.

## Facts that shape the design (Claude connector docs, read 2026-11-23)

- Claude connects to remote MCP servers **from Anthropic's cloud**
  (egress 160.79.104.0/21), so the server needs a **public HTTPS URL**.
  A server on a home network needs a tunnel.
- Transport: **Streamable HTTP** (recommended; SSE is being deprecated).
  stdio only works for local desktop/Code use.
- Auth for an individual's custom connector: **OAuth 2.0 authorization code
  + PKCE (S256)**, with the client identified by Claude's published client
  metadata document (CIMD), dynamic client registration (DCR), or a
  pre-registered client id. Static bearer headers are a limited beta, and
  "no auth" is not acceptable for a server that can unlock a car.
- Discovery: unauthenticated requests get `401` with
  `WWW-Authenticate: Bearer resource_metadata="…"`; the protected-resource
  metadata's `resource` must equal the connector URL exactly.
- Connectors added on claude.ai are available in the mobile apps.
- Claude offers per-tool approval ("Needs approval" / "Always allow" /
  "Blocked"). Every tool must declare `readOnlyHint` and `destructiveHint`.
  Elicitation is not documented as supported, so the server can't ask the
  user to confirm by itself.
- Tool calls time out after 240 s; results are capped at ~150k characters.
- Anthropic "MCP tunnels" are Enterprise-only (research preview): not an
  option for a personal account.

## Goals

- Every `hkconnect` capability as an MCP tool, with honest, structured results
  (what the car actually did, how fresh the data is).
- Safe by default for a remotely reachable server that can unlock a car.
- One small binary, low memory, runs on a Raspberry Pi, a Mac, or a $5 VPS.
- Works locally over stdio for testing with Claude Desktop / Claude Code.

## Non-goals

- Multi-user / multi-account hosting. One owner, one Bluelink account.
- A web UI beyond the OAuth sign-in page.
- Push notifications (no server-initiated messages to Claude).

## Anti-goals

- A server anyone on the internet can call.
- Claude starting the engine or unlocking without the owner explicitly
  allowing that tool.
- Waking the car on a schedule (battery drain): `status` uses cached data
  unless the caller explicitly asks to refresh.

## Shape

New workspace crate `crates/hkc-mcp` (binary `hkc-mcp`), so the CLI does not
pull in a web server.

```
hkc-mcp stdio                 # local: Claude Desktop / Claude Code
hkc-mcp serve --listen 127.0.0.1:8787 --public-url https://car.example.com
hkc-mcp set-password          # set/rotate the sign-in password (hashed)
hkc-mcp revoke-all            # drop every issued token
```

Built on `rmcp` (official Rust MCP SDK, 3.11, MSRV 1.88) with `axum` for the
HTTP side. Configuration (Bluelink credentials, vehicle, tool policy) uses the
same `HKC_*` variables / config-dir `.env` as `hkc`.

### Tools

| Tool | Hints | Notes |
|---|---|---|
| `list_vehicles` | read-only | |
| `get_status` | read-only | `refresh: bool` (default false; wakes the car, rate-limited server-side to e.g. once per 10 min) |
| `get_location` | read-only | includes age of the fix and a map link |
| `get_vehicle_info` | read-only | capabilities, odometer, service intervals |
| `get_trips` | read-only | month summary / day listing / one trip's route + events |
| `check_command` | read-only | result of an earlier command by id |
| `lock` | destructive=false | waits for the car's answer |
| `unlock` | destructive | **disabled unless enabled in policy** |
| `start_climate` | destructive | temp, minutes, defrost; **disabled unless enabled** |
| `stop_climate` | destructive=false | needs PIN |
| `hazard_lights` | destructive | horn optional |
| `set_valet_mode` | destructive | |
| `send_destination` | destructive=false | name, lat, lon, address |

Commands wait up to ~90 s for the car (within Claude's 240 s limit) and
return `{command_id, outcome, seconds}`; on timeout they return the id and
tell Claude to use `check_command` rather than resend. Commands are never
retried automatically (the library already guarantees this).

Tool results are compact JSON plus a one-line human summary, with timestamps
in IST and an explicit `data_age` so Claude doesn't present hours-old cached
status as current.

### Safety layers

1. **OAuth** (remote only): nobody without the owner's password can call any
   tool.
2. **Server-side tool policy** (`HKC_MCP_ENABLE=unlock,start_climate,…`):
   unlock and remote start are off unless listed. Disabled tools are not
   advertised at all.
3. **Claude's per-tool approval**: the README tells the owner to set
   destructive tools to "Needs approval".
4. **Audit log**: every command (tool, args, OAuth client, outcome) appended
   to a local file, never including secrets.

### OAuth server (single user)

- Endpoints: `/.well-known/oauth-protected-resource/mcp`,
  `/.well-known/oauth-authorization-server`, `/authorize` (sign-in page),
  `/token`, optional `/register` (DCR); MCP at `/mcp`.
- Clients: accept CIMD client ids (fetch + validate the metadata document,
  https only, redirect URI must exactly match one listed), plus DCR.
  Optionally restrict to Claude's published client id.
- PKCE S256 required; authorization codes single-use, 60 s lifetime, bound to
  client id + redirect URI + code challenge.
- Sign-in page: one password field. Password stored as an Argon2id hash
  (set via `hkc-mcp set-password`). Constant-time checks, CSRF token, attempt
  rate limiting with backoff, no account enumeration.
- Tokens: opaque random 256-bit; stored hashed (SHA-256) with expiry; access
  1 h, refresh 30 days with rotation and reuse detection. `revoke-all` wipes
  them. State persisted in a 0600 file in the data dir.
- Strict `resource` / audience check on every request.

### Hosting (open decision)

| Option | Notes |
|---|---|
| Always-on box at home + Cloudflare Tunnel | Free; no open ports; credentials stay at home |
| Small VPS + Caddy (auto HTTPS) | ~$4–6/mo; always up |
| Fly.io-style container + volume | ~$0–3/mo; platform-specific |

Whichever is chosen, the repo ships: a release binary per platform, a
`Dockerfile`, a `systemd` unit / launchd plist, and a step-by-step guide.

## Testing

- Tool layer: unit tests against a mocked `hkconnect` (wiremock fixtures, as
  in the library).
- OAuth: integration tests for the full flow (CIMD + DCR, PKCE, code reuse,
  wrong password, token expiry/refresh/rotation, wrong resource).
- MCP protocol: drive the server with `rmcp`'s client over stdio and HTTP.
- Manual: connect from Claude Desktop (stdio), then claude.ai + phone.

## Open questions

- Hosting location (owner's decision).
- Whether to restrict OAuth clients to Claude's published client id only
  (safer) or allow any MCP client (more flexible).
