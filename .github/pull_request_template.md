## What and why

<!-- What does this change, and why? Link related issues. -->

## How it was tested

<!-- Tests added or run. If you tried it on a real car, give the model, year and fuel type. -->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (no warnings) and `cargo test --workspace` pass
- [ ] New or changed fixtures are scrubbed: no real VIN, ids, email, tokens, modem ids or coordinates
- [ ] No secrets, `.env` or `session.json` committed; no new logging of credentials, tokens or request bodies
- [ ] `docs/api-coverage.md` and the README updated if endpoints or commands changed
- [ ] `CHANGELOG.md` updated under `[Unreleased]` for user-visible changes

By submitting this pull request, I agree that my contribution is dual licensed under MIT OR Apache-2.0.
