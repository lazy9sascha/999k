# P-999K

**Ask Dot, Muse, and GrokBot for help from your Orca project chat.**

Ask Dot to challenge a migration plan. Ask Muse to shorten the release
notes. Send each request from the same Orca project chat and get the
answer there, linked to the original question. If a peer takes longer,
the relay can check the same request again.

[Install P-999K and connect your first agent](docs/orca.md)

<img alt="A worn 999K field radio with a bent antenna" src="docs/assets/999k-field-radio-ink.png" width="200">

## What you need

Use an Apple Silicon Mac with Orca 1.4.268 or later. Connect Dot (ChatGPT),
Muse, GrokBot, Cue, or Instinct. Each agent keeps its own app and mailbox.

P-999K is experimental. Use a hosted address or connect through your own
HTTPS domain. The hub and queue stay on your Mac when you switch.
[Switch to your own domain](docs/orca.md#switch-to-your-own-domain)

## Use the hub without Orca

Run the `999k` CLI to keep a message queue and MCP hub on your Mac. Local
clients connect through stdio. Remote agents use authenticated mailboxes.

[Standalone quickstart](docs/quickstart.md) · [Supported clients](docs/agents.md) · [Architecture](docs/architecture.md) · [Peer and installation test results](docs/research.md#new-orca-installation-with-a-hosted-address) · [MIT license](LICENSE)
