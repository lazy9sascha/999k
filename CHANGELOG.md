# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## 0.2.2

- Explain that P-999K is experimental before hosted registration. Describe what Cloudflare and Supabase handle, which data stays on the Mac, and when public access can expire.
- Add an own-domain guide to the Orca setup panel and management bot. Reuse the same hub and queue when switching a peer from a hosted address to an HTTPS endpoint you control.
- Show hosted registration status in the CLI. Let operators inspect Cloudflare resources and track a dedicated Dot Access app with the installation so its route survives updates and the app is removed during revocation or expiry.

## 0.2.1

The native CLI and the P-999K Orca plugin both use version 0.2.1.

- Add the P-999K Orca plugin at version 0.2.1. The catalog ID is `lazy9sascha.p-999k`. The repository is `lazy9sascha/p-999k`.
- The plugin requires Orca 1.4.268 or newer and a native `999k` build with `--orca-peer` and `pair`. It opens setup for one relay bot per existing peer.
- The CLI can check a linked reply by original ID. Pending and invalid IDs return an unsuccessful status.
- A project agent can ask a bound peer. **Queued** means the hub accepted the request. The relay returns only the reply linked to that request, and that reply can arrive later.
- The plugin does not open the peer’s existing mailbox route. Muse still polls `/muse/mcp` over HTTPS through the hub’s tunnel.

- Updated native builds advertise MCP tools with the `p-999k_` prefix. Calls using `radio999k_` or `pigeonmq_` still work. Installing the Orca plugin alone keeps the tool names of the installed native hub and CLI.

## 0.2.0 - 2026-11-24

- PigeonMQ is now **999K**. New examples use the `999k` executable and the `radio999k_*` mailbox tools.
- The release packages target Apple Silicon Macs and Linux on x86-64 and ARM64.
- Connect a local MCP client to the running hub with `999k stdio-mcp`. The hub authenticates the mailbox and uses the existing queue.
- Enable the private socket on an existing managed macOS hub with `init <active-profile> --apply --stdio-socket`.
- Set up Orca relay bots for existing peers with the separate 999K plugin and `init --orca`. Each bot's local tool sends to one peer and retrieves replies linked to its original request.

## 0.1.6 (candidate)

- Build native archives with both release executables and their checksums.
- Generate direct, npm, and Homebrew installer candidates from the same pinned archives.
- Add a loopback smoke test and an install-to-MCP Quickstart.
- Native Linux and Intel Mac checks remain pending. Installer assets and wrappers are not published.

## 0.1.5

- Run an existing private hub, its secure tunnels, and its email relay under [one macOS LaunchAgent](docs/tunnels.md#run-a-private-hub-with-one-launchagent).
- Reject a second hub for the same SQLite queue, even on a different port.
- Reject invalid static relay mailbox lists before startup.

## 0.1.4

- Publish the first PigeonMQ release on GitHub Releases.
- This release changes the version and release notes only. It adds no runtime changes after 0.1.3.

## 0.1.3

- Dot Events notify the existing subscription about ordinary messages and linked terminal replies.
- Grok Bot wake covers ordinary messages and linked terminal replies.
- An optional local notifier passes new arrival identifiers to an owner-selected command.
- On macOS, `init NAME` stages a built-in profile; `init NAME --apply` starts or updates the managed hub. Repeating the same setup preserves the Bearer and leaves running services alone.
- Cue and Instinct can opt in separately to one shared email relay. Email carries only the new message ID; agents still read and reply through MCP.
- A mailbox can be retired and staged again with a new Bearer and isolated message history.
- The hub keeps queue schema 4. Older queues without the generation catalog can keep serving their existing profiles, but managed activation and removal are unavailable. See [older queue databases](docs/troubleshooting.md#older-queue-databases).

## 0.1.2

- `pigeonmq_inbox` lists received mail newest first by default. Set `scope: all` for sent and received mail, add `query` to search either view, or set `order: oldest` to reverse the list.
- The same arguments are available on Dot, Codex, Muse, Grok, and read-enabled registered mailbox routes. Upgrades keep the existing queue. Start a new page sequence and refresh saved apps using the [upgrade checklist](docs/troubleshooting.md#refresh-a-saved-mcp-app).

## 0.1.1

- The Cargo package and hub executable are now named `pigeonmq`.
- When upgrading from 0.1.0, change `target/release/pigeonmq-cloud` to `target/release/pigeonmq` in scripts and service definitions. Keep the same `--home` path to reuse the queue and credentials.
- Document how Dot lists registered Codex mailboxes and sends to a selected local session.

## 0.1.0 - 2026-11-19

First public release. A macOS hub for direct agent-to-agent messaging: one SQLite queue, per-agent MCP routes, and Bearer auth.

- `init <agent>` stages an isolated local mailbox. When a matching triage record is present, init reads it and stages only the allowed local tools.
- `run --env-file PATH` reads private tunnel settings from a file. Tunnel providers are registered; tunnl.gg is the current built-in provider.
- A tunnel mode is required. tunnl.gg is opt-in.
- `run` and `serve-hub` accept `--profiles-file`. Mailboxes staged by `init` are mounted with `--mailbox-home`.
- `dlq-inspect` inspects one quarantined request without writing. The same inspect path is available over the read-only `/messages/dlq` HTTP API.
- Built-in routes serve official rmcp Streamable HTTP.
- A profile manifest can set `wake_files_dir` for Grok Bot wake files.
