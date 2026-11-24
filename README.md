# hkconnect

<img src="docs/assets/icon.svg" width="96" height="96" align="right" alt="hkconnect icon: a padlock under a signal arc">

An unofficial async Rust client, command-line tool and Claude (MCP) server for
Hyundai Bluelink India and Kia Connect India connected cars.

[![CI](https://github.com/SwapShaOnline/hkconnect/actions/workflows/ci.yml/badge.svg)](https://github.com/SwapShaOnline/hkconnect/actions/workflows/ci.yml)
[![hkconnect on crates.io](https://img.shields.io/crates/v/hkconnect?label=hkconnect)](https://crates.io/crates/hkconnect)
[![hkc on crates.io](https://img.shields.io/crates/v/hkc?label=hkc)](https://crates.io/crates/hkc)
[![hkc-mcp on crates.io](https://img.shields.io/crates/v/hkc-mcp?label=hkc-mcp)](https://crates.io/crates/hkc-mcp)
[![docs.rs](https://img.shields.io/docsrs/hkconnect)](https://docs.rs/hkconnect)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV 1.89](https://img.shields.io/badge/MSRV-1.89-orange.svg)](#minimum-supported-rust-version)

| Crate | What it is |
|---|---|
| [`hkconnect`](crates/hkconnect) | Async library: login, session reuse, typed reads, remote commands |
| [`hkc`](crates/hkc) | Command-line app built on the library |
| [`hkc-mcp`](crates/hkc-mcp) | MCP server so Claude can read and control the car ([setup guide](docs/mcp.md), or [deploy on Railway](docs/railway.md)) |

An agent skill, [`skills/hkconnect`](skills/hkconnect/SKILL.md),
tells Claude Code and other agents when and how to use `hkc-mcp` (or `hkc`)
safely.

**Claude Code in one step.** With `hkc-mcp` installed (see [Install](#install))
and your login in its [`.env` file](#configuration), this adds the skill and
the MCP server together (Claude Code 2.1.275 or later):

```
/plugin install hkconnect --marketplace SwapShaOnline/hkconnect
```

Other agents: copy `skills/hkconnect` into their skills folder and add the
server as described in the [setup guide](docs/mcp.md).

- MCP Registry name: `mcp-name: io.github.SwapShaOnline/hkc-mcp`

> **Disclaimer.** This project is not affiliated with, endorsed by, or
> supported by Hyundai Motor Company, Kia Corporation, or their Indian
> subsidiaries. It uses the same undocumented servers as the official apps,
> with your own account. The API can change or break without notice. Remote
> commands act on a real vehicle; you are responsible for how you use them.
> The software is provided "as is", without warranty of any kind.

## Support

| Area | Status |
|---|---|
| Hyundai India and Kia India, older (non-CCS2) protocol | Supported |
| Reads: vehicles, status, location, info, trips | Verified on a real car (2024 Hyundai Creta, petrol, automatic) |
| Lock | Verified on a real car (2024 Hyundai Creta): confirmed by the car in about 30 s |
| Remote start with climate, stop | Verified on a real car (2024 Hyundai Creta): start confirmed in about 33 s, stop in about 15 s |
| Unlock | Verified on a real car (2024 Hyundai Creta): confirmed by the car in about 12 s |
| Send destination to navigation | Verified on a real car (2024 Hyundai Creta): confirmed by the car in about 24 s |
| Valet mode | Works on a real car (2024 Hyundai Creta): on and off both took effect, but the server never reports the result, so `hkc` ends with "unknown" after waiting; check the car's screen |
| Hazard lights / horn | Experimental: implemented to match the reference implementation and tested against a fake server; not yet verified on a real car, and the endpoint may not exist on non-CCS2 cars |
| Newer "CCS2" cars | Not supported: status and remote commands return a clear error |
| EV features (charging, charge limits, schedules) | Not implemented |
| Other regions (EU, US, Canada, ...) | Out of scope; see [hyundai_kia_connect_api](https://github.com/Hyundai-Kia-Connect/hyundai_kia_connect_api) |

Kia support uses the same protocol as Hyundai on a different host and has
not been tested against a real Kia. The endpoint-by-endpoint status is in
[`docs/api-coverage.md`](docs/api-coverage.md). Reports from other models
are welcome: open an issue with the "New car / endpoint support" form.

## Install

**No local setup:** deploy `hkc-mcp` to your own Railway account and connect
it to Claude ([guide](docs/railway.md)):

[![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/cSgHE3)

Prebuilt binaries of `hkc` (the CLI) and `hkc-mcp` (the MCP server) are
published on [GitHub Releases](https://github.com/SwapShaOnline/hkconnect/releases)
for macOS (Apple silicon and Intel), Linux (x86_64 and arm64, glibc) and
Windows (x86_64). Pick one of:

**Homebrew** (macOS, Linux):

```sh
brew install SwapShaOnline/tap/hkc
brew install SwapShaOnline/tap/hkc-mcp
```

**Shell installer** (macOS, Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-mcp-installer.sh | sh
```

**PowerShell installer** (Windows):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-installer.ps1 | iex"
powershell -ExecutionPolicy Bypass -c "irm https://github.com/SwapShaOnline/hkconnect/releases/latest/download/hkc-mcp-installer.ps1 | iex"
```

**cargo-binstall** (downloads the prebuilt binary):

```sh
cargo binstall hkc
cargo binstall hkc-mcp
```

**From crates.io** (builds from source; needs Rust 1.89 or later):

```sh
cargo install hkc
cargo install hkc-mcp
```

From a git checkout:

```sh
git clone https://github.com/SwapShaOnline/hkconnect
cd hkconnect
cargo install --path crates/hkc
```

To use the library, add it to your project along with an async runtime:

```sh
cargo add hkconnect
cargo add tokio --features macros,rt-multi-thread
```

## Configuration

`hkc` reads your Bluelink / Kia Connect app login from environment
variables:

| Variable | Required | Meaning |
|---|---|---|
| `HKC_USERNAME` | yes | Account email |
| `HKC_PASSWORD` | yes | Account password |
| `HKC_PIN` | for PIN commands | The 4-digit PIN set in the app (stop, hazard, valet, navigate) |
| `HKC_BRAND` | no | `hyundai` (default) or `kia` |
| `HKC_VEHICLE` | if several cars | Which car, by name, VIN or id (same as `--vehicle`) |

Instead of exporting them, you can put them in a `.env` file at:

- macOS: `~/Library/Application Support/hkconnect/.env`
- Linux: `~/.config/hkconnect/.env`

```sh
HKC_USERNAME=you@example.com
HKC_PASSWORD=change-me
HKC_PIN=0000
HKC_BRAND=hyundai
```

The file is read literally, one `KEY=VALUE` per line:

- Lines starting with `#`, and blank lines, are ignored. An `export ` prefix
  is allowed.
- Values are never expanded: `$`, `\` and `${...}` are kept as written, so
  `HKC_PASSWORD=pa$word1` means exactly `pa$word1`.
- Spaces around the value are trimmed. To keep them, or to use `#`, wrap the
  value in single or double quotes (`HKC_PASSWORD='my # pass'`); the quotes
  are removed and nothing inside is interpreted. A quoted value cannot
  contain its own kind of quote, so use the other kind. A comment may follow
  the closing quote.
- An unquoted value containing ` #` is rejected rather than guessed at
  (quote it, or move the comment to its own line), as are lines without `=`
  and unterminated quotes. Rejected lines are reported by line number, never
  by value, and skipped.

A `.env` in the current directory is deliberately not read. Real environment
variables and command-line options take precedence over the file. Restrict
the file's permissions (`chmod 600`), since it contains your password; `hkc`
warns if other users can read it.

After the first login, `hkc` saves the session (access and refresh tokens,
no password) as `session.json` in the OS data directory
(`~/Library/Application Support/hkconnect/` on macOS,
`~/.local/share/hkconnect/` on Linux) with owner-only (0600) permissions,
so later commands skip logging in again. `hkc logout` deletes it. If
the session cannot be saved, `hkc` prints a warning but keeps the command's
own result and exit code.

## Usage

```sh
hkc vehicles                 # cars on the account
hkc status                   # last reported state (fast; does not wake the car)
hkc status --refresh         # wake the car for fresh data (uses the 12V battery)
hkc location                 # where the car is parked
hkc info                     # remote features, odometer, service intervals
hkc trips --help             # trip history: monthly and daily summaries, trip details

hkc lock
hkc unlock                   # asks for confirmation (or pass --yes)
hkc start -t 22 -m 10        # remote start with climate at 22 °C for 10 min; asks for confirmation
hkc start -t 18 --defrost --rear-defrost --yes
hkc stop                     # needs HKC_PIN
hkc hazard                   # flash the hazard lights; needs HKC_PIN; experimental
hkc hazard --horn            # with the horn; asks for confirmation
hkc valet on                 # valet mode on/off; asks for confirmation; needs HKC_PIN; result not reported (see Support)
hkc navigate --help          # send a destination to the car's navigation; asks for confirmation; needs HKC_PIN

hkc bench                    # API latency, read-only (see docs/performance.md)
hkc --json status            # machine-readable output
hkc -c "My Creta" status     # pick a car (required when the account has several)
hkc -v status                # show API calls (never secrets)
```

### Shell completions and man page

`hkc` prints its own tab-completion scripts (bash, zsh, fish, elvish,
PowerShell) and manual page:

```sh
# zsh (add `fpath=(~/.zfunc $fpath)` before `compinit` in ~/.zshrc)
mkdir -p ~/.zfunc && hkc completions zsh > ~/.zfunc/_hkc

# bash
mkdir -p ~/.local/share/bash-completion/completions
hkc completions bash > ~/.local/share/bash-completion/completions/hkc

# fish
hkc completions fish > ~/.config/fish/completions/hkc.fish

# man page
mkdir -p ~/.local/share/man/man1 && hkc man > ~/.local/share/man/man1/hkc.1
```

Remote start accepts temperatures from 14 to 29.5 °C in 0.5 steps and
durations of 1 to 10 minutes; anything else is rejected before you are asked
to confirm.

If the account has more than one car, every command except `hkc vehicles`
needs `--vehicle` (or `HKC_VEHICLE`). It matches an exact id first, then a
VIN, then a name (both ignoring case); a name shared by two cars is rejected,
so use the VIN or id.

Remote commands wait for the car to report the result, which usually takes
10 to 40 seconds. Pass `--no-wait` to return as soon as the server accepts
the command, then look the result up later with `hkc check <id>`. The
command id is printed as soon as the server accepts the command. With
`--json`, the final result goes to stdout once the car answers, and while
waiting `hkc` writes one line to stderr as soon as the command is accepted:
`{"event":"sent","command":"Unlock","id":"..."}`, so a script that is
killed while waiting still knows the id. `hkc` exits non-zero if the car reports failure or does
not respond, or if waiting fails or is interrupted with Ctrl-C; the command
was still sent, so check it with `hkc check <id>` rather than sending it
again.

Run `hkc --help` or `hkc <command> --help` for every option.

## Library quick start

```rust,no_run
use std::time::Duration;

use hkconnect::{Brand, Celsius, Client, ClimateOptions, CommandStatus, Config, Credentials};

#[tokio::main]
async fn main() -> hkconnect::Result<()> {
    let client = Client::new(
        Config::india(Brand::Hyundai),
        Credentials::new("you@example.com", "password").with_pin("0000"),
    )?;

    // Reads.
    let car = client.vehicles().await?.remove(0);
    let status = client.status(&car).await?; // cached; does not wake the car
    println!("{} locked: {:?}, as of {:?}", car.name, status.locked, status.updated_at);

    // Options are validated when built: 14 to 29.5 °C, 1 to 10 minutes.
    let options = ClimateOptions::new(Celsius::new(21.5)?).with_duration_minutes(5)?;
    // Commands return as soon as the server accepts them...
    let id = match client.start_climate(&car, &options).await {
        Ok(id) => id,
        // Don't blindly resend an unlock or a remote start: check first.
        Err(e) if e.may_have_been_received() => {
            eprintln!("{e}; the car may still act on it: check its status before retrying");
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    // ...and the car answers 10 to 40 seconds later.
    let outcome = client
        .wait_for(&car, &id, Duration::from_secs(90), Duration::from_secs(3))
        .await?;
    if outcome != CommandStatus::Succeeded {
        eprintln!("remote start did not complete: {outcome:?}");
    }

    // Save `client.session().await` somewhere private and pass it to
    // `Client::with_session` next time to skip the login.
    Ok(())
}
```

`Client` logs in on first use, refreshes tokens automatically, and is safe
to share across tasks (wrap it in an `Arc`). Errors map the server's result
codes to [`hkconnect::Error`](https://docs.rs/hkconnect/latest/hkconnect/enum.Error.html)
variants such as `RateLimited`, `CarTimeout` and `InvalidPin`. The returned
data types (`Vehicle`, `VehicleStatus`, `Trip`, ...) implement serde's
`Serialize` and `Deserialize` with the same field names both ways, so you
can store them and read them back; a stored `Trip` can still be passed to
`Client::trip_detail`. Full API documentation is on
[docs.rs](https://docs.rs/hkconnect).

## Safety

- **Confirmations.** `hkc unlock`, `hkc start`, `hkc hazard --horn`,
  `hkc valet` and `hkc navigate` name the car and ask before acting, and
  refuse to run non-interactively without `--yes`. Think before scripting
  them. `lock`, `stop` and `hazard` without `--horn` do not ask.
- **The right car.** With several cars on the account, `hkc` refuses to
  guess: pick one with `--vehicle` or `HKC_VEHICLE`.
- **"Sent" is not "done".** The server accepting a command only means it was
  queued. `hkc` waits for the car's own result and reports success, failure,
  or no response.
- **Don't resend blindly.** If sending fails with a network error or
  timeout, the server may still have received the command; run
  `hkc status` before retrying. If the command was accepted but waiting
  fails, `hkc` prints its id: use `hkc check <id>` instead of sending it
  again.
- **12V battery.** `hkc status --refresh` (and `Client::refresh_status`)
  wakes the car's modem and draws on the 12V battery. Do not poll it on a
  timer; plain `hkc status` returns the last reported state without waking
  the car.
- **Rate limits.** Hyundai and Kia rate-limit accounts (error 5091). Space
  out requests, especially commands and refreshes.
- **PIN.** Stop, hazard, valet and navigate need the app PIN, which is
  exchanged for a short-lived control token. Repeated wrong PINs may
  block remote control on your account for a while, so once the server
  rejects the PIN, the library never submits it again for the life of that
  `Client` (for `hkc`, the rest of the run). Check `HKC_PIN` if you see
  "the PIN was rejected".
- **Experimental commands.** Until a command is verified on a real car, test
  it while you can see the car.

## Privacy

- Credentials, tokens and the PIN are held as redacted secrets and never
  logged. `-v` shows each call's method, path and HTTP status; paths
  include your vehicle id, so redact it before sharing the output.
- The saved session lets anyone who can read it control your car until the
  tokens expire. Keep your home directory private, and run `hkc logout` on
  shared machines.
- Trip history includes GPS routes and timestamps, and `hkc location` reports
  where the car is parked. Treat this output as personal data before sharing
  or storing it.
- `hkc` talks only to the brand's official servers. It sends nothing
  anywhere else.
- Text from the server (car names, service items, error messages) is
  stripped of control characters before `hkc` prints it, so it cannot
  inject terminal escape sequences.

When filing issues, redact your email, VIN, vehicle id, tokens and
coordinates.

## How it works

The client registers a device, logs in through the same OAuth flow as the
Android app, and calls the regional REST API. Commands return a message id;
the outcome is read later from the account's notification records. The
protocol, design decisions and open questions are documented in
[`docs/design.md`](docs/design.md), and every endpoint's status is listed in
[`docs/api-coverage.md`](docs/api-coverage.md).

## Minimum supported Rust version

Rust 1.89 (edition 2024). Raising the MSRV is treated as a minor-version
change.

## Contributing

Bug reports, reports from other car models, and pull requests are welcome.
See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, quality gates and how to
add an endpoint, and [SECURITY.md](SECURITY.md) to report a vulnerability
privately. Changes are recorded in [CHANGELOG.md](CHANGELOG.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Credits

The protocol knowledge in this project (endpoints, request formats, app
identifiers and error codes) comes from
[hyundai_kia_connect_api](https://github.com/Hyundai-Kia-Connect/hyundai_kia_connect_api),
an MIT-licensed Python library by Fuat Akgun and contributors. See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
