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

[![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/cSgHE3)

The template builds this repository's
[`crates/hkc-mcp/deploy/Dockerfile`](../crates/hkc-mcp/deploy/Dockerfile)
(the first build compiles from source and takes several minutes), adds a
volume at `/data`, and asks for these variables:

| Variable | What to enter |
|---|---|
| `HKC_USERNAME` | Your Bluelink / Kia Connect email |
| `HKC_PASSWORD` | Your Bluelink / Kia Connect password |
| `HKC_PIN` | The 4-digit PIN from the app (needed for stop, hazard, valet, destination) |
| `HKC_BRAND` | `hyundai` (default) or `kia` |
| `HKC_MCP_ENABLE` | Optional extra tools, e.g. `unlock,start_climate` (see [docs/mcp.md](mcp.md), section 3). Leave empty for the safe defaults. |

The template also sets:

| Variable | Value | Why |
|---|---|---|
| `HKC_MCP_PASSWORD` | a random 32-character value, generated at deploy | The password for your server's sign-in page (see [section 2](#2-add-it-to-claude)). You may replace it with your own (12+ characters); it is not your Bluelink password. |
| `RAILWAY_RUN_UID` | `0` | Railway mounts volumes owned by root; the image's own user couldn't write to it. Leave it. |

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
3. Click **Connect**. On the sign-in page, check the addresses are
   Claude's (`claude.ai` or `claude.com`). For the password, copy
   `HKC_MCP_PASSWORD` from the service's **Variables** tab on Railway
   (click the eye icon, then copy), paste it and allow. Keep it secret:
   with it, anyone can connect to your car through this server.
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
- **Updates:** Railway tells you when this repository's `main` changes
  (the template is built from it); accept the update, or redeploy, to
  rebuild with the new code.

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

The template is live at <https://railway.com/deploy/cSgHE3>. These are the
settings it was created with, from the maintainer's Railway account
(**Workspace → Templates → New Template**), for reference when editing it. The template is built from the
GitHub repository rather than the ghcr.io image because only
repository-based templates notify deployers of updates.

**Service**

- Source: GitHub repository `SwapShaOnline/hkconnect`, branch `main`, root
  directory empty (the repository root: the Dockerfile copies the whole
  workspace).
- Build: Railway reads [`railway.json`](../railway.json) from the
  repository root: Dockerfile builder with
  `crates/hkc-mcp/deploy/Dockerfile`, rebuilds only when `crates/`,
  `Cargo.toml`, `Cargo.lock` or the Dockerfile change, healthcheck
  `/healthz` (60 s), restart on failure (10 retries). Nothing to enter.
- Service name: `hkc-mcp`.

**Variables** (name = default; description shown to the deployer)

| Name | Default | Description |
|---|---|---|
| `HKC_USERNAME` | (empty, required) | Your Bluelink / Kia Connect email. |
| `HKC_PASSWORD` | (empty, required) | Your Bluelink / Kia Connect password. Stored only in this service's variables. |
| `HKC_PIN` | (empty, optional) | The 4-digit PIN from the app; needed for stop climate, hazard lights, valet mode and sending a destination. |
| `HKC_BRAND` | `hyundai` | `hyundai` or `kia`. |
| `HKC_MCP_PASSWORD` | `${{secret(32)}}` | Password for this server's sign-in page when you connect Claude. Generated for you: copy it from the Variables tab when Claude asks. Not your Bluelink password. 12+ characters if you set your own. |
| `HKC_MCP_ENABLE` | (empty, optional) | Extra tools beyond the safe defaults, comma-separated, e.g. `unlock,start_climate`, or `all`. Leave empty unless you need them. |
| `RAILWAY_RUN_UID` | `0` | Leave as is: Railway volumes are owned by root. |
| `PORT` | `8080` | Port the server listens on; matches the public HTTP proxy. Leave as is. |

`HKC_MCP_DATA_DIR=/data` is already set by the image, so it isn't needed.

**Volume:** mount path `/data` (holds the Bluelink session, the OAuth
state and the audit log).

**Networking:** public networking on: an HTTP proxy to port 8080 (the
`PORT` variable above, which the server listens on), with a generated
Railway domain. No TCP
proxy. A custom domain works too; then also set `HKC_MCP_PUBLIC_URL` to
`https://` plus that domain.

**Deploy settings:** one replica (the OAuth state is a file on the
volume); region is up to the deployer (Singapore is closest to India).

The template's deploy link is `https://railway.com/deploy/cSgHE3`, used by
the buttons here and in `README.md`. It is not listed on Railway's template
marketplace; **Publish to Marketplace** on the template page would list it.

### The ghcr.io image (optional)

Releases also push a multi-arch image, `ghcr.io/swapshaonline/hkc-mcp`
(`.github/workflows/publish-image.yml`). Railway doesn't need it: a
service can be switched to **Docker image** source with
`ghcr.io/swapshaonline/hkc-mcp:latest` to skip the build, at the cost of
update notifications (then redeploy to pull a new `latest`). The package
is private after its first push; make it public in its package settings
for anyone to pull it.
