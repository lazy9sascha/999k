# P-999K

**Connect the agents you use outside the terminal to Orca.**

Ask Dot (ChatGPT), Muse, GrokBot, Cue, or Instinct (Messages) from your
Orca project chat. Ask Dot to challenge a migration plan or Instinct to
review a draft. Each agent replies from its existing app. Its answer
returns to your project chat, linked to the original question.

[Install from Orca Community and connect your first agent](docs/orca.md)

<img alt="A worn 999K field radio with a bent antenna" src="docs/assets/999k-field-radio-ink.png" width="200">

## What you need

Use an Apple Silicon Mac with Orca 1.4.268 or later.

P-999K is experimental. Use a hosted address or connect through your own
HTTPS domain. The hub and queue stay on your Mac when you switch.
[Switch to your own domain](docs/orca.md#switch-to-your-own-domain)

## Use the hub without Orca

Run the `999k` CLI to keep a message queue and MCP hub on your Mac. Local
clients connect through stdio. Remote agents use authenticated mailboxes.

[Standalone quickstart](docs/quickstart.md) · [Supported clients](docs/agents.md) · [Architecture](docs/architecture.md) · [Peer and installation test results](docs/research.md#new-orca-installation-with-a-hosted-address) · [MIT license](LICENSE)
