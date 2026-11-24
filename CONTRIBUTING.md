# Contributing to hkconnect

Thanks for helping. Bug reports, reports from car models we have not seen,
and pull requests are all welcome. For security problems, follow
[SECURITY.md](SECURITY.md) instead of opening a public issue.

## Development setup

You need a Rust toolchain at or above the MSRV (1.89) with `rustfmt` and
`clippy`:

```sh
rustup component add rustfmt clippy
git clone https://github.com/SwapShaOnline/hkconnect
cd hkconnect
cargo build --workspace
```

The workspace has two crates:

- `crates/hkconnect`: the library. Protocol, models and the `Client`.
- `crates/hkc`: the CLI (binary `hkc`), a thin layer over the library.

[`docs/design.md`](docs/design.md) explains the protocol and the design
choices; [`docs/api-coverage.md`](docs/api-coverage.md) lists every endpoint
and its status.

You do not need a car or an account to work on the code: the test suite runs
against a local fake server. To try your changes against your own car, set
the `HKC_*` variables described in the [README](README.md#configuration) and
run `cargo run -p hkc -- status`. Prefer read commands; test remote commands
only while you can see the car.

## Quality gates

CI runs these on every pull request; run them locally first:

```sh
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Clippy runs with the `pedantic` group enabled at the workspace level. If a
lint is wrong for a specific case, silence it locally with
`#[expect(clippy::..., reason = "...")]` and a real reason rather than
disabling it crate-wide. CI also checks that the workspace builds on the
MSRV.

Tests never touch a real car or the real servers. Keep it that way: any test
that needs network access must go through `wiremock`.

## Adding support for a new endpoint

1. **Capture.** Record a real request and response for the endpoint, from
   your own account: for example with `hkc -v` plus a temporary debug print,
   or by reading the reference implementation
   ([hyundai_kia_connect_api](https://github.com/Hyundai-Kia-Connect/hyundai_kia_connect_api))
   and calling the endpoint once. Do not hammer the car: one capture is
   enough, and avoid endpoints that wake the car unless that is the point.
2. **Scrub.** Remove personal data following the rules below. Do this before
   the file is ever staged.
3. **Fixture.** Save the scrubbed response as
   `crates/hkconnect/tests/fixtures/<name>.json`, keeping the full envelope
   (`retCode`, `resCode`, `resMsg`, `msgId`) and the server's field names and
   value formats exactly as received.
4. **Typed model.** Add a model in `crates/hkconnect/src/` (see `model.rs` and
   `trips.rs`). The server's JSON is read through a private "wire" struct
   whose fields are `Option` with `#[serde(default)]` and the lenient helpers
   in `de.rs` (string-or-number values, IST timestamps, `lenient` nested
   objects, `lenient_vec` lists that skip bad entries); unknown fields are
   ignored. It is then converted into a public `#[non_exhaustive]` type that
   derives `Serialize` and `Deserialize` symmetrically with its own
   snake_case field names (add it to the round-trip test). Expose a
   method on `Client` with a doc comment saying what it does, whether it
   wakes the car, and whether it needs the PIN. Commands return a
   `CommandId`.
5. **Wiremock test.** Add a test in `crates/hkconnect/tests/api.rs` that
   mounts the fixture on a `MockServer`, checks the request (method, path,
   headers, and the exact JSON body for commands), and asserts on the parsed
   result. Cover the obvious error paths (for example a `retCode: "F"`
   envelope) when the endpoint has its own failure modes.
6. **CLI and docs.** If it belongs in `hkc`, add the subcommand with `--json`
   output, a confirmation for anything that unlocks or starts the car, and
   `--no-wait` for commands. Update `docs/api-coverage.md`, the README
   support table if relevant, and `CHANGELOG.md` under `[Unreleased]`.

Mark a command "Implemented, unverified" in the coverage table until someone
has confirmed it on a real car, and say which car in the pull request.

## Fixture scrubbing rules

Every fixture is public forever once merged. Before adding or changing one,
replace:

| Data | Replace with |
|---|---|
| VIN | `MALTEST0000000001` (keep the 17-character length) |
| Vehicle id, device id, user id, and other account UUIDs | `00000000-0000-4000-8000-000000000001` style placeholders, consistent within the file |
| Email, phone number, name | `owner@example.com`, `0000000000`, `Owner` |
| Access, refresh and control tokens, authorization codes, cookies | `REDACTED` or obviously fake values such as `access-1` |
| Modem, SIM and telematics unit identifiers (IMEI, ICCID, MSISDN, serial numbers) | Zeros of the same length |
| Coordinates (parking location, trip start and end, route points) | Synthetic points near a public landmark (the existing fixtures use central Bengaluru), not your real places |
| Registration (licence plate) number, dealer and service history details | Fake values |

Also check free-text fields, nicknames, and nested objects. Search the file
for your email, VIN, plate and the first digits of your home latitude before
committing. Message ids (`msgId`) and timestamps may stay.

## Secrets

- Never commit a `.env` file, a `session.json`, or any file containing a
  password, PIN or token. `.env` is in `.gitignore`; do not force-add it.
- Never paste tokens, VINs, vehicle ids or coordinates into issues, pull
  requests or commit messages. `hkc -v` output includes your vehicle id in
  request paths; redact it.
- Do not add logging that could print credentials, tokens, the PIN, cookies
  or request bodies. Secrets are held in `secrecy::SecretString`; keep it that
  way.

## Commits and pull requests

- Keep pull requests focused: one feature or fix each.
- Write commit messages in the imperative mood ("Add valet mode"), with a
  short summary line and a body explaining why when it is not obvious.
- Describe how you tested the change. For behaviour that depends on a real
  car, say which model, year and fuel type you tried it on, or that you
  could not.
- Add an entry to `CHANGELOG.md` under `[Unreleased]` for user-visible
  changes.
- Make sure all quality gates pass.

## Releasing

All three crates share one version, set once in the root `Cargo.toml`.
Releases are built and published by [dist](https://opensource.axo.dev/cargo-dist/)
from a version tag; nothing is published from a developer machine.

1. Bump `version` under `[workspace.package]` in `Cargo.toml`, and the
   `hkconnect = { version = "..." }` dependency in `crates/hkc/Cargo.toml`
   and `crates/hkc-mcp/Cargo.toml` to the same value. Also bump `version` in
   `.claude-plugin/plugin.json` (Claude Code plugin users only get updates
   when it changes) and both `version` fields in `server.json` (MCP Registry).
   `cargo test` fails until all of them match.
2. Move the `[Unreleased]` entries in `CHANGELOG.md` under a new
   `## [X.Y.Z] - YYYY-MM-DD` heading.
3. Run the quality gates, then commit (`Release vX.Y.Z`) and push to `main`.
4. Tag and push the tag:

   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

Pushing the tag runs `.github/workflows/release.yml`, which:

- plans the release and fails straight away if the tag does not match the
  workspace version;
- builds `hkc` and `hkc-mcp` for macOS (arm64, x86_64), Linux (x86_64,
  arm64) and Windows (x86_64), plus shell and PowerShell installers;
- creates the GitHub Release with the archives, checksums and installers
  (these archives are also what `cargo binstall` downloads);
- pushes the `hkc` and `hkc-mcp` formulae to
  [SwapShaOnline/homebrew-tap](https://github.com/SwapShaOnline/homebrew-tap);
- publishes `hkconnect`, then `hkc`, then `hkc-mcp` to crates.io
  (`.github/workflows/publish-crates.yml`), skipping any crate whose
  version is already there, so a failed release can be re-run.

A tag with a pre-release suffix (`v0.2.0-rc.1`) makes a GitHub pre-release
and skips Homebrew and crates.io.

`release.yml` is generated by dist from `dist-workspace.toml`. Do not edit it
by hand: change the config and run `dist generate` (CI checks that the file
is up to date). `publish-crates.yml` is ours and can be edited freely.

After crates.io has the new `hkc-mcp`, publish the release to the
[MCP Registry](https://registry.modelcontextprotocol.io) from the repository
root. The registry checks the `mcp-name:` line in the crate's README on
crates.io, so the release must include it (the README has it):

```sh
brew install mcp-publisher   # once
mcp-publisher login github   # once per machine; device-code sign-in
mcp-publisher publish        # reads server.json
```

### One-time setup

- **Homebrew tap token.** Create a fine-grained personal access token with
  access to the `SwapShaOnline/homebrew-tap` repository only and the
  "Contents: Read and write" permission. Save it as the repository secret
  `HOMEBREW_TAP_TOKEN` (Settings, Secrets and variables, Actions).
- **`release` environment.** Create an environment named `release`
  (Settings, Environments). The crates.io job runs in it; optionally add
  protection rules such as required reviewers or "only tags matching `v*`".
- **crates.io Trusted Publishing.** For each of `hkconnect`, `hkc` and
  `hkc-mcp`, open the crate's Settings on crates.io, Trusted Publishing, and
  add a GitHub publisher with owner `SwapShaOnline`, repository `hkconnect`,
  workflow filename `release.yml` and environment `release`. The workflow
  filename is the top-level workflow (`release.yml`), not the reusable
  `publish-crates.yml` it calls: crates.io checks the token's `workflow_ref`,
  which names the caller. No crates.io API token is stored anywhere.

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as MIT OR Apache-2.0, without any additional
terms or conditions.
