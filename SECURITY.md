# Security policy

hkconnect handles account credentials and tokens that can unlock and start
a real car, so security reports are taken seriously.

## Reporting a vulnerability

Please report vulnerabilities privately, not in a public issue, pull request
or discussion.

Use GitHub's private vulnerability reporting: go to the repository's
[Security tab](https://github.com/swapsha96/hkconnect/security) and choose
**Report a vulnerability**
([direct link](https://github.com/swapsha96/hkconnect/security/advisories/new)).

Include what you found, how to reproduce it, the affected version, and the
impact you expect. Do not include your own real credentials, tokens, VIN or
location; use placeholders.

You can expect an acknowledgement within a week. Once a fix is ready, it will
be released and a GitHub security advisory published, crediting you unless
you prefer otherwise. This is a volunteer project, so please allow
reasonable time before disclosing publicly.

## Scope

In scope:

- Handling of credentials, the PIN, access, refresh and control tokens:
  anything that could expose them in logs, error messages, `Debug` output,
  `--json` output, panics or crash reports.
- The saved session file (`session.json`): its permissions, location,
  atomic writes, and anything that could let another local user read or
  replace it.
- `.env` loading in `hkc` (only the config directory is read).
- Anything that could make `hkc` act on the car without the confirmation it
  promises, or report a command as succeeded when it did not.
- TLS and transport: requests sent to a host other than the brand's API, or
  without certificate verification.
- Vulnerable dependencies that are reachable from hkconnect's code.

Out of scope:

- Vulnerabilities in Hyundai's or Kia's servers, apps or vehicles. Report
  those to the manufacturer.
- The app identifiers and keys in `crates/hkconnect/src/config.rs`. They are
  shipped in the official Android apps, identify the app rather than a user,
  and are public.
- Attacks that require an attacker who already controls your user account
  or reads your files as you.
- Extra logging you enable yourself with `RUST_LOG` for third-party crates
  (for example HTTP client tracing), which may print request headers.

## Keeping yourself safe

- Do not paste tokens, passwords, the PIN, your VIN, vehicle id, email, or
  coordinates into issues, pull requests, chat or logs you share. Output of
  `hkc -v` contains your vehicle id; trip history and `hkc location` contain
  where you have been.
- Keep your `.env` file owner-readable only (`chmod 600`) and never commit it.
- Run `hkc logout` on shared machines; the saved session can control the car
  until its tokens expire.
- If you think your tokens leaked, run `hkc logout` and change your password
  in the official app.
- Install `hkc` from crates.io or this repository, not from third-party
  binaries.

## Supported versions

Only the latest released version receives security fixes.
