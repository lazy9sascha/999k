# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## 0.1.0 - 2026-11-19

First public release. A macOS hub for direct agent-to-agent messaging: one SQLite queue, per-agent MCP routes, and Bearer auth.

- `init <agent>` stages an isolated local mailbox. When a matching triage record is present, init reads it and stages only the allowed local tools.
- `run --env-file PATH` reads private tunnel settings from a file. Tunnel providers are registered; tunnl.gg is the current built-in provider.
- A tunnel mode is required. tunnl.gg is opt-in.
- `run` and `serve-hub` accept `--profiles-file`. Mailboxes staged by `init` are mounted with `--mailbox-home`.
- `dlq-inspect` inspects one quarantined request without writing. The same inspect path is available over the read-only `/messages/dlq` HTTP API.
- Built-in routes serve official rmcp Streamable HTTP.
- A profile manifest can set `wake_files_dir` for Grok Bot wake files.
