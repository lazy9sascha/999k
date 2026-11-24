# Security policy

999K exposes a queue on your Mac to cloud agents through a tunnel, so its auth checks matter. Thank you for reporting problems privately.

## Supported versions

| Version | Supported |
| --- | --- |
| 0.1.x (latest commit) | ✅ yes |

## Report a vulnerability

- Do not disclose vulnerability details in a public issue.
- If the Security tab offers **Report a vulnerability**, use it. Otherwise, ask the maintainer for a private channel in a general issue. Keep vulnerability details private.
- Include the commit, macOS version, the route or command involved, steps to reproduce, and what an attacker could do.
- Remove tokens, webhook URLs and headers, live tunnel URLs, and message bodies first. Use placeholders such as `https://<name>.tunnl.gg`.

## In scope

- Bearer authentication, mailbox and operation policy, and Host/Origin checks on the hub routes
- Signed Events callbacks and the webhook wake
- Token storage and handoff (see [credential custody](docs/configuration.md#credential-custody-and-handoff))
- Anything that lets a request reach the queue without a valid token

The [auth model](docs/architecture.md#auth-model) describes what the hub checks. Known gaps are listed in [limitations](docs/architecture.md#limitations).

## If a token leaks

Reissue it with `credentials ... reissue` ([commands](docs/configuration.md#commands)), then update the agent's connector. To cut a peer off, set `revoked: true` for it in the [registration file](docs/configuration.md#registration-format).
