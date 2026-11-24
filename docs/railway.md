# Run hkc-mcp on Railway

[Railway](https://railway.com) can host `hkc-mcp serve` for you, so Claude
(claude.ai and the mobile apps) can reach your car without a computer at
home, a tunnel or a reverse proxy. You need a Railway account (it bills
by usage; a server this small costs very little) and your Bluelink / Kia
Connect login.

> Railway has no region in India; the nearest is Singapore. Whether
> Hyundai / Kia India's servers answer from there has not been tested yet.
> If the server's log shows the car calls timing out, run it at home
> instead ([docs/mcp.md](mcp.md), section 5).

Read the warning at the top of [docs/mcp.md](mcp.md) first: this server
controls a real car.

## 1. Deploy

[![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/TEMPLATE_CODE)

<!-- TEMPLATE_CODE is a placeholder until the template is published; see
     "For the maintainer" below. -->

The template runs the prebuilt image `ghcr.io/swapshaonline/hkc-mcp:latest`
with a volume at `/data` and asks for these variables:

| Variable | What to enter |
|---|---|
| `HKC_USERNAME` | Your Bluelink / Kia Connect email |
| `HKC_PASSWORD` | Your Bluelink / Kia Connect password |
| `HKC_PIN` | The 4-digit PIN from the app (needed for stop, hazard, valet, destination) |
| `HKC_BRAND` | `hyundai` or `kia` |
| `HKC_MCP_PASSWORD` | A **new** password (12+ characters; a passphrase of several words is easiest) that you'll type on your server's sign-in page when Claude connects. Not your Bluelink password. |
| `HKC_MCP_ENABLE` | Optional extra tools, e.g. `unlock,start_climate` (see [docs/mcp.md](mcp.md), section 3). Leave empty for the safe defaults. |

The template also sets, and you should leave alone:

| Variable | Value | Why |
|---|---|---|
| `HKC_MCP_DATA_DIR` | `/data` | Where the volume is mounted |
| `RAILWAY_RUN_UID` | `0` | Railway mounts volumes owned by root; the image's own user couldn't write to it |

You don't need to set a port, a public URL or proxy settings: on Railway
`hkc-mcp` listens on `0.0.0.0:$PORT`, uses `https://$RAILWAY_PUBLIC_DOMAIN`
as its public URL (unless `HKC_MCP_PUBLIC_URL` is set, e.g. for a custom
domain) and takes the client address for rate limiting from Railway's
proxy.

After the deploy, open the service's **Settings → Networking** and click
**Generate Domain** if it doesn't have one yet (the server refuses to start
without a public URL). Then check it:

```sh
curl -i https://YOUR-SERVICE.up.railway.app/mcp
```

should answer `401` with a `WWW-Authenticate: Bearer resource_metadata="…"`
header.

## 2. Add it to Claude

1. On claude.ai: **Settings → Connectors → Add custom connector**.
2. Name: e.g. "Car". URL: `https://YOUR-SERVICE.up.railway.app/mcp`.
   Leave the OAuth fields empty.
3. Click **Connect**, check the addresses on the sign-in page are Claude's
   (`claude.ai` or `claude.com`), enter your `HKC_MCP_PASSWORD` and allow.
4. Set the tool permissions; anything that unlocks or starts the car
   should be **"Needs approval"**.

Then **restrict the server to Claude**: in the service's logs, find the
line `sign-in page shown client_id=…`, and add a variable
`HKC_MCP_ALLOWED_CLIENTS` with that client id. Railway redeploys the
service; your connection stays signed in. Details are in
[docs/mcp.md](mcp.md), section 6.

## 3. Changing things later

- **Sign-in password:** change `HKC_MCP_PASSWORD`. On its next start the
  server notices the stored password differs, replaces it and signs every
  client out (the log says so, without the password); reconnect in Claude.
  While `HKC_MCP_PASSWORD` is set it is the password: a password set with
  `hkc-mcp set-password` would be replaced at the next start. To manage it
  with `set-password` instead, delete the variable (the stored password
  stays).
- **Tools:** change `HKC_MCP_ENABLE` / `HKC_MCP_DISABLE`, then refresh the
  connector in Claude.
- **Sign everyone out:** change `HKC_MCP_PASSWORD`, or run
  `hkc-mcp revoke-all` in the service's shell.
- **Updates:** redeploy the service to pull the newest `latest` image.

The volume keeps the Bluelink session, the OAuth state (password hash and
token hashes) and the audit log `mcp-audit.jsonl` across restarts and
redeploys. Don't delete it unless you want to start over.

Proxy trust: on Railway, `X-Forwarded-For` is believed from any peer,
because Railway's edge proxy is the only way in from the internet and its
addresses aren't published. Other services in the same Railway project can
reach this one over the private network and could choose the address the
rate limits apply to; don't share the project with services you don't
trust. Set `HKC_MCP_TRUST_PROXY=false` (or list addresses in
`HKC_MCP_TRUSTED_PROXIES`) to change this.

## For the maintainer: publishing the template

These steps are done once, from the maintainer's Railway account:

1. Make the GitHub Container Registry package
   `ghcr.io/swapshaonline/hkc-mcp` **public** (GitHub → your packages →
   hkc-mcp → Package settings → Change visibility). It is pushed by the
   release workflow (`.github/workflows/publish-image.yml`) on every
   release tag, and is private until changed.
2. On Railway: **Templates → New template**. Add a service with source
   **Docker image** `ghcr.io/swapshaonline/hkc-mcp:latest`.
3. Variables: `HKC_USERNAME`, `HKC_PASSWORD`, `HKC_PIN`, `HKC_MCP_PASSWORD`
   (required, no default), `HKC_BRAND` (default `hyundai`),
   `HKC_MCP_ENABLE` (optional, empty), `HKC_MCP_DATA_DIR=/data`,
   `RAILWAY_RUN_UID=0`. Add descriptions from the table above.
4. Add a volume mounted at `/data`. Enable a public domain (HTTP, Railway
   picks the port from `$PORT`).
5. Settings: healthcheck path `/healthz`, restart policy "on failure", one
   replica (the OAuth state is a file on the volume).
6. Publish, and replace `TEMPLATE_CODE` in the button above (and in
   `README.md`) with the template's code.

Building from the repository instead of the image also works: Railway
picks up `railway.json` at the repository root, which points at
`crates/hkc-mcp/deploy/Dockerfile` and sets the same healthcheck. That
compiles from source on each deploy (several minutes), so the image is the
default.
