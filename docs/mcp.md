# hkc-mcp: use your car from Claude

`hkc-mcp` is an [MCP](https://modelcontextprotocol.io) server that lets Claude
read and control your Hyundai or Kia (India) car through the `hkconnect`
library: "is the car locked?", "where is it parked?", "how far did I drive
this week?", "lock it", and, if you allow it, "start the AC".

It runs in two ways:

| Mode | For | Auth |
|---|---|---|
| `hkc-mcp stdio` | Claude Desktop and Claude Code on the same computer | none needed (local process) |
| `hkc-mcp serve` | claude.ai custom connector, which also works in the Claude mobile apps | OAuth with a password only you know |

> **Read this first.** This is unofficial software using the same servers as
> the official app, with your account. Remote commands act on a real car;
> lock, remote start and stop have been verified on one so far (see the
> README's support table). Anyone who can call the server's
> command tools can do what the tools allow, so keep the password strong,
> keep unlock and remote start off unless you need them, and set them to
> "Needs approval" in Claude.

## Contents

1. [Install](#1-install)
2. [Configure your Bluelink account](#2-configure-your-bluelink-account)
3. [Choose which tools Claude gets](#3-choose-which-tools-claude-gets)
4. [Local use: Claude Desktop and Claude Code](#4-local-use-claude-desktop-and-claude-code)
5. [Remote use: claude.ai and the mobile apps](#5-remote-use-claudeai-and-the-mobile-apps)
6. [Add the connector in Claude](#6-add-the-connector-in-claude)
7. [Revoking access, rotating the password](#7-revoking-access-rotating-the-password)
8. [Reference](#8-reference)

## 1. Install

Homebrew (macOS, Linux):

```sh
brew install SwapShaOnline/tap/hkc-mcp
```

Installer script (macOS, Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-mcp-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-mcp-installer.ps1 | iex"
```

With Rust 1.89 or newer: `cargo binstall hkc-mcp` (prebuilt) or
`cargo install hkc-mcp` (from source), or from a checkout: `cargo build --release -p hkc-mcp` (the binary is
`target/release/hkc-mcp`). A container image can be built with
`crates/hkc-mcp/deploy/Dockerfile` (see [Docker](#docker)).

## 2. Configure your Bluelink account

`hkc-mcp` reads the same settings as the `hkc` command-line tool: from the
environment, or from a `.env` file in the config directory:

- macOS: `~/Library/Application Support/hkconnect/.env`
- Linux: `~/.config/hkconnect/.env`

```sh
HKC_USERNAME=you@example.com
HKC_PASSWORD='your Bluelink password'
HKC_PIN=1234            # the 4-digit PIN from the app; needed for stop, hazard, valet, destination
HKC_BRAND=hyundai       # or kia
# HKC_VEHICLE=Creta     # default car if the account has several
```

Make it private: `chmod 600 ~/.config/hkconnect/.env` (or the macOS path).
Values are taken literally: `$` and `\` are not special. Quote a value
that contains ` #`. A real environment variable wins over the file.

If Bluelink rejects the email or password, `hkc-mcp` stops trying to sign
in for 5 minutes (doubling after each further rejection, up to 6 hours) so
that a wrong password cannot get the account locked. Fix the settings and
restart `hkc-mcp`.

The Bluelink login session is saved to `mcp-session.json` in the data
directory (mode 0600), separate from `hkc`'s session.

## 3. Choose which tools Claude gets

| Tool | What it does | Annotations | Default |
|---|---|---|---|
| `list_vehicles` | Cars on the account | read-only | on |
| `get_status` | Lock state, doors, engine, climate, fuel, warnings; cached unless `refresh=true` | read-only | on |
| `get_location` | Last parked position, its age, and a map link | read-only | on |
| `get_vehicle_info` | Model, features, odometer, service intervals | read-only | on |
| `get_trips` | Month summary, a day's trips (`with_events=true`: plus every trip's driving events, up to 20 trips, one request each), or one trip's totals and events (`include_route=true`: plus its GPS route, up to 400 points) | read-only | on |
| `check_command` | Result of an earlier command, by id | read-only | on |
| `lock` | Lock the doors | not destructive | on |
| `stop_climate` | Stop a remote start (needs PIN) | not destructive | on |
| `send_destination` | Send a place to the car's navigation (needs PIN) | not destructive | on |
| `unlock` | Unlock the doors | **destructive** | **off** |
| `start_climate` | Remote start with climate control | **destructive** | **off** |
| `hazard_lights` | Flash hazard lights, optionally horn (needs PIN) | **destructive** | **off** |
| `set_valet_mode` | Valet mode on/off (needs PIN) | **destructive** | **off** |

A tool that is off is not advertised at all: Claude cannot see or call it.
Turn tools on or off with comma-separated lists:

```sh
HKC_MCP_ENABLE=unlock,start_climate     # or "all"
HKC_MCP_DISABLE=send_destination
```

Restart `hkc-mcp` after changing them. Unknown names are an error, so a typo
cannot silently leave something on. (To change the defaults in code, edit
`DEFAULT_ON` and `OPT_IN` in `crates/hkc-mcp/src/policy.rs`.)

How the tools behave:

- **Freshness.** `get_status` and `get_location` return the car's last
  report, with `as_of` (India Standard Time) and `data_age`. `refresh=true`
  wakes the car for live data; this drains the 12V battery, so the server
  allows it at most once per car every 10 minutes
  (`HKC_MCP_REFRESH_MINUTES`, 0 to disable the limit). A refused refresh
  returns cached data and says so.
- **Commands** are sent once and never retried. The tool then waits up to
  90 s (`HKC_MCP_COMMAND_WAIT_SECONDS`, at most 170) for the car's answer and
  returns `{command_id, outcome, seconds}`. If the car hasn't answered, the
  result tells Claude to use `check_command` rather than send it again. If
  sending failed in a way that leaves it unclear whether the server got the
  command, the result says not to resend.
- **Several cars.** Every tool takes an optional `vehicle` (name, VIN or id).
  With several cars and no `vehicle` (and no `HKC_VEHICLE`), the tool
  returns the list to choose from.
- **Audit log.** Every tool call, reads included (status, location, trips,
  ...), is appended to `mcp-audit.jsonl` in the data directory (mode 0600):
  time, tool, arguments, the OAuth client id (or `stdio`), the car's VIN,
  the command id (for commands and `check_command`) and the outcome
  (`succeeded`, `failed`, `pending`, `not_sent`, ...; for reads `ok`,
  `cached`, `refreshed`, `refresh_skipped` or `error`). Results are never
  written, so the car's coordinates and routes are not in the log (the
  arguments of `send_destination` are, since you chose them). Passwords,
  PINs and tokens are never written.
- **Data from Hyundai's servers.** Tool results contain text that comes
  from Bluelink / Kia Connect (car names, error messages). Control and
  bidirectional-override characters are removed from it, but it is still
  someone else's text: treat it as data, not as instructions.

## 4. Local use: Claude Desktop and Claude Code

No network port and no password are involved: Claude starts `hkc-mcp stdio`
as a child process.

**Claude Desktop**: Settings → Developer → Edit Config, then add to
`claude_desktop_config.json` (use the full path to the binary; `which hkc-mcp`
prints it: typically `/opt/homebrew/bin/hkc-mcp` with Homebrew on Apple
Silicon, `~/.cargo/bin/hkc-mcp` with the installer script or Cargo):

```json
{
  "mcpServers": {
    "car": {
      "command": "/Users/you/.cargo/bin/hkc-mcp",
      "args": ["stdio"]
    }
  }
}
```

The credentials come from the `.env` file above. You can also pass settings
in an `"env": { "HKC_MCP_ENABLE": "start_climate" }` object, but don't put
the Bluelink password in this file. Restart Claude Desktop.

**Claude Code**:

```sh
claude mcp add car -- "$(which hkc-mcp)" stdio
```

Optionally, install the agent skill so Claude knows when to use these tools,
to report how old the data is, and never to resend a command:

```sh
cp -R skills/hkconnect-car ~/.claude/skills/   # from a checkout of this repo
```

## 5. Remote use: claude.ai and the mobile apps

Custom connectors added on claude.ai are called **from Anthropic's servers**,
so `hkc-mcp serve` needs a public **https** URL. It speaks plain HTTP on a
local port; put a TLS-terminating tunnel or reverse proxy in front.

### 5.1 Set the sign-in password

Claude will send you to a sign-in page on your server the first time you
connect. Set its password (at least 12 characters; a passphrase of several
words is easiest):

```sh
hkc-mcp set-password
```

It is stored only as an Argon2id hash in `mcp-oauth.json` in the data
directory (mode 0600). `serve` refuses to start without one. For scripts and
containers: `hkc-mcp set-password --password-stdin`.

### 5.2 Run the server

```sh
hkc-mcp serve --listen 127.0.0.1:8787 --public-url https://car.example.com --trust-proxy
```

- `--public-url` is the origin Claude will use; the MCP endpoint is
  `https://car.example.com/mcp`. It must match exactly what you enter in
  Claude, and requests must arrive with that `Host` header (both proxies
  below pass it through).
- `--trust-proxy` makes rate limiting use the client address from the last
  `X-Forwarded-For` entry, but only for connections that come from
  loopback (a proxy or tunnel on the same machine, as in both setups
  below). A connection from anywhere else is limited by its own address,
  whatever header it sends, so the header can't be forged from outside.
  If your proxy connects from another address (another machine, or the
  Docker bridge; see [Docker](#docker)), list it with
  `--trusted-proxies 172.17.0.1` (comma-separated addresses or CIDR
  networks; implies `--trust-proxy`). Keep that list as narrow as you can:
  anything on it can choose the address the limits apply to.
- Every option also has an environment variable (`HKC_MCP_LISTEN`,
  `HKC_MCP_PUBLIC_URL`, `HKC_MCP_TRUST_PROXY=true`,
  `HKC_MCP_TRUSTED_PROXIES`); see the [reference](#8-reference).

Choose one of the following.

#### Option A: a computer at home + Cloudflare Tunnel

No open ports on your router; your Bluelink credentials stay at home. You
need a domain on Cloudflare (free plan is fine) and `cloudflared`.

```sh
cloudflared tunnel login
cloudflared tunnel create hkc-mcp
cloudflared tunnel route dns hkc-mcp car.example.com
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: hkc-mcp
credentials-file: /home/you/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: car.example.com
    service: http://127.0.0.1:8787
  - service: http_status:404
```

Run `cloudflared tunnel run hkc-mcp` (or install it as a service with
`cloudflared service install`), and run `hkc-mcp serve` as above, as a
service (see [Running as a service](#running-as-a-service)).

Don't put Cloudflare Access (or a bot challenge) in front of this hostname:
Claude's servers must reach `/mcp`, `/token` and the `/.well-known/`
documents without a browser. `hkc-mcp` does its own authentication.

#### Option B: a small VPS + Caddy

Caddy gets and renews the certificate automatically. Point a DNS record for
`car.example.com` at the VPS, open ports 80 and 443, and use this
`Caddyfile`:

```
car.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Keep `hkc-mcp` on `127.0.0.1` so it is reachable only through Caddy. Caddy
streams server-sent events without buffering, which `/mcp` needs.

#### Running as a service

- Linux: `crates/hkc-mcp/deploy/hkc-mcp.service` (systemd, with hardening;
  instructions at the top of the file).
- macOS: `crates/hkc-mcp/deploy/com.github.hkconnect.hkc-mcp.plist`
  (launchd agent).

#### Docker

```sh
docker build -f crates/hkc-mcp/deploy/Dockerfile -t hkc-mcp .
docker volume create hkc-mcp-data
printf '%s\n' 'your sign-in passphrase' | \
  docker run --rm -i -v hkc-mcp-data:/data hkc-mcp set-password --password-stdin
docker run -d --name hkc-mcp --restart unless-stopped \
  -v hkc-mcp-data:/data --env-file ./hkc-mcp.env \
  -p 127.0.0.1:8787:8787 hkc-mcp
```

`hkc-mcp.env` holds `HKC_USERNAME`, `HKC_PASSWORD`, `HKC_PIN`,
`HKC_MCP_PUBLIC_URL=https://car.example.com`, `HKC_MCP_ALLOWED_CLIENTS` (see
[section 6](#6-add-the-connector-in-claude)) and any tool settings (keep it
mode 0600).

Inside the container the server has to listen on `0.0.0.0:8787`. Always
publish it **only on the host's loopback**, `-p 127.0.0.1:8787:8787` as
above, and put the tunnel or proxy (on the host) in front. Never use
`-p 8787:8787`: that serves plain HTTP to your whole network.

Seen from inside the container, the host's proxy connects from the Docker
bridge gateway, not from loopback, so `X-Forwarded-For` is ignored by
default and every visitor shares one rate-limit address (safe, but an
attacker's wrong passwords then also delay yours). To have per-visitor
limits, find the gateway (`docker network inspect bridge -f
'{{(index .IPAM.Config 0).Gateway}}'`, usually `172.17.0.1`) and add
`HKC_MCP_TRUSTED_PROXIES=172.17.0.1` to `hkc-mcp.env`. Don't list a whole
range other containers or hosts can connect from.

### 5.3 Check it from outside

```sh
curl -i https://car.example.com/mcp
```

should answer `401` with a `WWW-Authenticate: Bearer resource_metadata="…"`
header, and

```sh
curl https://car.example.com/.well-known/oauth-protected-resource/mcp
```

should show `"resource": "https://car.example.com/mcp"`.

## 6. Add the connector in Claude

1. On claude.ai: **Settings → Connectors → Add custom connector**.
2. Name: anything, e.g. "Car". URL: `https://car.example.com/mcp`. Leave the
   OAuth client fields empty.
3. Click **Connect**. A page on your server asks for the `hkc-mcp` password.
   It shows, in large type, **the address the app comes from** (the host of
   its client id URL) and **where your browser will return**; both should
   be Claude's addresses (`claude.ai` or `claude.com`). The name the app
   gives itself is shown only as "calls itself …": any app can choose any
   name, so don't go by it. If the two addresses differ, the page says so
   in red; unless you expected exactly that, close the page. Enter the
   password and allow.
4. In the connector's settings, set the tool permissions:
   - read tools: "Always allow" is reasonable;
   - `lock`, `stop_climate`, `send_destination`: your choice;
   - `unlock`, `start_climate`, `hazard_lights`, `set_valet_mode` (if you
     enabled them): **"Needs approval"**, so Claude asks you before every
     use.

Connectors added on claude.ai are available in the Claude mobile apps too.

To allow `unlock` or remote start later: add them to `HKC_MCP_ENABLE`,
restart `hkc-mcp`, then in Claude refresh or reconnect the connector so it
sees the new tools, and set them to "Needs approval".

**Strongly recommended: allow only Claude.** By default any MCP client
can show you a sign-in page, so a phishing link could ask for your password
on your own server on behalf of someone else's app (the page shows that
app's address, but it's easy to miss). Claude identifies itself with a
client id URL (a Client ID Metadata Document). Right after your first
successful connection, find it in the server log (`sign-in page shown
client_id=…`) and restrict the server to it, then restart:

```sh
HKC_MCP_ALLOWED_CLIENTS=<the client id from the log>
```

Until you do, `hkc-mcp serve` prints a warning at startup.

**Dynamic client registration** (`--allow-dcr` / `HKC_MCP_ALLOW_DCR=true`)
is off by default. Turn it on only for a client that can't use a client id
URL (for example some versions of the MCP Inspector).

## 7. Revoking access, rotating the password

```sh
hkc-mcp revoke-all            # every client must sign in again
hkc-mcp revoke-all --clients  # also forget dynamically registered clients
hkc-mcp set-password          # new password; also revokes all tokens
                              # (--keep-tokens to keep them)
```

Both work while the server runs (they edit the state file it reads on every
request). Also remove the connector in Claude if you no longer use it.

If you suspect someone else has your sign-in password: run `set-password`,
check `mcp-audit.jsonl` for calls you didn't make (it lists every tool call,
including reads of the car's location and trips, with the client id that
made it), and change your Bluelink password and PIN in the official app
too.

## 8. Reference

### Commands

```
hkc-mcp stdio                                   # MCP over stdin/stdout
hkc-mcp serve --public-url URL [--listen ADDR] [--trust-proxy]
              [--trusted-proxies CIDRS] [--allow-dcr] [--allowed-clients IDS]
hkc-mcp set-password [--password-stdin] [--keep-tokens]
hkc-mcp revoke-all [--clients]
```

`stdio` and `serve` also take `--brand`, `--vehicle`, `--enable`,
`--disable`, `--refresh-minutes`, `--command-wait-seconds` and `--verbose`.
A flag wins over the environment, which wins over the `.env` file.

### Settings

| Variable | Default | Meaning |
|---|---|---|
| `HKC_USERNAME`, `HKC_PASSWORD` | (required) | Bluelink / Kia Connect login |
| `HKC_PIN` | none | App PIN; needed for stop, hazard, valet, destination |
| `HKC_BRAND` | `hyundai` | `hyundai` or `kia` |
| `HKC_VEHICLE` | none | Default car (name, VIN or id) |
| `HKC_MCP_ENABLE` | none | Extra tools, comma-separated, or `all` |
| `HKC_MCP_DISABLE` | none | Default tools to hide |
| `HKC_MCP_REFRESH_MINUTES` | `10` | Minimum minutes between live refreshes of one car (0 = no limit) |
| `HKC_MCP_COMMAND_WAIT_SECONDS` | `90` | How long commands wait for the car (1–170) |
| `HKC_MCP_DATA_DIR` | platform data dir | Session, OAuth state, audit log |
| `HKC_MCP_LISTEN` | `127.0.0.1:8787` | `serve`: listen address |
| `HKC_MCP_PUBLIC_URL` | (required for `serve`) | Public https origin |
| `HKC_MCP_TRUST_PROXY` | `false` | Use `X-Forwarded-For` (from loopback peers) for rate limiting |
| `HKC_MCP_TRUSTED_PROXIES` | none | More peers whose `X-Forwarded-For` is believed (addresses/CIDRs); implies `HKC_MCP_TRUST_PROXY` |
| `HKC_MCP_ALLOW_DCR` | `false` | Enable `/register` |
| `HKC_MCP_ALLOWED_CLIENTS` | any (warning at startup) | Only these OAuth client ids; strongly recommended |
| `RUST_LOG` | `hkc_mcp=info,warn` | Log filter (logs go to stderr) |

### Files (in the data directory)

| File | Contents |
|---|---|
| `mcp-session.json` | Bluelink login session (tokens) |
| `mcp-oauth.json` | Sign-in password hash, SHA-256 hashes of issued tokens, registered clients |
| `mcp-oauth.json.lock` | Empty; locked while the server or a subcommand changes `mcp-oauth.json` |
| `mcp-audit.jsonl` | One JSON line per tool call |

All are created owner-only (0600) in an owner-only directory. Data
directory: `~/Library/Application Support/hkconnect/` on macOS,
`~/.local/share/hkconnect/` on Linux.

### Security model (summary)

- **OAuth 2.1, single user.** Authorization code + PKCE (S256 required);
  codes are single-use, valid 60 s, bound to the client, redirect URI and
  challenge, and a replayed code revokes the tokens it produced. Access
  tokens last 1 hour, refresh tokens 30 days and rotate on every use; reusing
  an old refresh token revokes that whole sign-in. Tokens are random 256-bit
  values, stored only as SHA-256 hashes, and only valid for
  `https://<host>/mcp`.
- **Sign-in page.** One password field, Argon2id hash, CSRF token bound to a
  cookie, no framing, no caching. Wrong passwords are limited per address
  (3 free, then 5 s doubling to 15 min), per sign-in page (5) and globally
  (30 per hour). Each attempt is counted *before* its password is hashed
  (and given back if it was right), so sending many at once doesn't get
  more guesses, and at most 2 hashes run at a time (further attempts get
  "busy, try again" immediately), which bounds memory use. A determined
  attacker can therefore lock you out of signing in for up to an hour.
  The limits are kept in memory: restarting the server clears them (an
  attacker can't restart it, but keep that in mind before restarting
  during an attack).
- **Other limits.** Failed `/token` requests are limited per address (10
  free, then 1 s doubling to 5 min; no global limit, so nobody can lock
  out your refreshes), and an invalid token never rewrites the state file.
  One address can keep at most 3 sign-in pages open (opening more discards
  its own oldest, never someone else's), and may make the server fetch at
  most 10 new client metadata documents per 10 minutes (60 overall, 4 at
  once).
- **Clients.** Client ID Metadata Documents are fetched only over https,
  only from public addresses (the resolved address is pinned), never
  through a proxy from the environment, without redirects, within 5 s and
  64 KiB. Every redirect URI they list must be https (or http on
  localhost) without a fragment, and the request's redirect URI must be
  one of them exactly. When a document can't be fetched or is invalid,
  the sign-in page says only that (details go to the server log), so it
  can't be used to probe other servers.
- **State file.** `revoke-all` and `set-password` take the same file lock
  as the server, so changes made while it runs are never lost.
- **Tool policy, Claude's approval setting, and the audit log** are the
  next layers; see sections 3 and 6.
