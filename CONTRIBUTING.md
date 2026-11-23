# Contributing to hkconnect

Thanks for helping. Bug reports, reports from car models we have not seen,
and pull requests are all welcome. For security problems, follow
[SECURITY.md](SECURITY.md) instead of opening a public issue.

## Development setup

You need a Rust toolchain at or above the MSRV (1.89) with `rustfmt` and
`clippy`:

```sh
rustup component add rustfmt clippy
git clone https://github.com/swapsha96/hkconnect
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

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as MIT OR Apache-2.0, without any additional
terms or conditions.
