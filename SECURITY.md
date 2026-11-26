# Security policy

999K lets cloud agents reach a queue on your Mac through authenticated routes. Report security issues privately.

## Supported versions

Use the [latest stable release](https://github.com/lazy9sascha/p-999k/releases/latest)
when reproducing an issue. Include the affected version in your report.

## Report a vulnerability

- Do not disclose vulnerability details in a public issue.
- If the Security tab offers **Report a vulnerability**, use it. Otherwise, ask the maintainer for a private channel in a general issue. Keep vulnerability details private.
- Include the commit, macOS version, the route or command involved, steps to reproduce, and what an attacker could do.
- Remove tokens, webhook URLs and headers, live tunnel URLs, and message bodies first. Use placeholders such as `https://<name>.tunnl.gg`.

## In scope

- Bearer authentication, mailbox and operation policy, and Host/Origin checks on the hub routes
- Signed Events callbacks and the webhook wake
- Token storage and handoff (see [credential custody](docs/configuration.md#credential-custody-and-handoff))
- Registration, pairing, and verification of signed Cloudflare Access assertions

The [auth model](docs/architecture.md#auth-model) describes what the hub checks. Known gaps are listed in [limitations](docs/architecture.md#limitations).

## If a token leaks

Reissue it with `credentials ... reissue` ([commands](docs/configuration.md#commands)), then update the agent's connector. To cut a peer off, set `revoked: true` for it in the [registration file](docs/configuration.md#registration-format).
