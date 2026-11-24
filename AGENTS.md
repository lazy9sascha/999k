# Instructions for coding agents

If this checkout sits inside a parent workspace with [workspace instructions](../AGENTS.md), follow them too.

999K runs a standalone queue and MCP hub on your Mac. Cloud agents connect through a tunnel. It owns its own queue and does not depend on a PigeonMQ Local installation at build time or runtime. Read [docs/architecture.md](docs/architecture.md) before changing behavior.

## Layout

```text
src/
  main.rs                 CLI (clap) entry point
  bin/                    999k-codex-receiver, 999k-dot-resolve
  lib.rs                  SQLite queue, leases, atomic reply + ACK
  mailbox/                Shared reply + ACK completion engine
  local_http.rs           Loopback HTTP hub and per-route dispatch
  local_http/transport.rs Loopback HTTP transport
  hub_http.rs             Loopback HTTP client
  auth.rs                 Bearer, mailbox, and operation policy
  cloud/                  `run`, `init`/`discover`/`setup-muse`, Keychain tokens;
                          runtime_profiles.rs, runtime_triage.rs
  mcp/catalog.rs          Shared mailbox profiles
  mcp/mailbox_tools.rs    MCP tool definitions
  adapters/
    muse/ grok_bot/ dot/  Per-agent profiles (grok_bot/grok_wake.rs: webhook wake)
    codex/                Codex mailbox, Events, and local native receiver
  events/                 Shared subscriptions, callbacks, and durable outbox
  receiver/               Common authenticated lease and completion API
tests/                    Integration tests
docs/                     Release documentation
skills/                   One setup skill per agent: skills/999k-<agent>/SKILL.md
schemas/                  Triage record schema and examples
```

## Build and test

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
```

## Rules

- **Secrets.** Never put tokens, webhook URLs or headers, live tunnel URLs, session IDs, or queue data in source, docs, logs, or chat. Use placeholders such as `https://<name>.tunnl.gg`.
- **Test data.** Use a temporary home, synthetic messages, and test tokens. Never point tests at a production queue or credentials.
- **Production.** Don't change a running hub, its home, its policies, or tunnels without the owner's approval. Don't push, publish, deploy, or open a live tunnel without approval.
- **Auth stays on.** Never disable or weaken Bearer, mailbox, or Host/Origin checks to make something pass.
- **Keep it small.** Reuse the existing queue, adapters, and tests. No new storage layer, daemon, or shared library for hypothetical needs.
- **Evidence.** Keep local tests, tunnel readiness, and real app results separate. A tool list proves discovery only. A webhook 200 proves a run started. It does not prove that the run finished.

## Docs

- `docs/` is for release readers. Plain, short English.
- When you run [agent triage](docs/architecture.md#agent-triage) on an agent, write `<home>/triage/<agent>.json` (the default home is `.local`) so that it matches [the triage schema](schemas/triage.schema.json). Update the [capability table](docs/agents.md#capability-table) and add a dated entry to the [research log](docs/research.md). Write `unknown` rather than guess. The file must not contain tokens or live hosts. See [Triage file to init](docs/architecture.md#triage-file-to-init) for the isolated staging rules.
- Update user docs when a command, flag, file, or behavior changes.
- Work logs, trial notes, and handoffs go in `.local/` (gitignored), not in `docs/`. Private source paths behind research labels are in `.local/research-sources.md`.
