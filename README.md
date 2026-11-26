# P-999K

<img alt="A worn 999K field radio with a bent antenna" src="docs/assets/999k-field-radio-ink.png" width="240">

Ask an agent you already use to help with an Orca project. For example,
enter “Ask Muse to review these install instructions.” The answer appears
in the same project chat.

## Connect your first agent

Use an Apple Silicon Mac with Orca 1.4.268 or later. Connect Dot, Muse,
GrokBot, Cue, or Instinct from **P-999K → Initial setup**.
[Install P-999K and connect a peer](docs/orca.md)

P-999K is experimental. Use a hosted address or connect through your own
HTTPS domain. The hub and queue stay on your Mac when you switch.
[Switch to your own domain](docs/orca.md#switch-to-your-own-domain)

## Use the hub without Orca

Run the `999k` CLI to keep a message queue and MCP hub on your Mac. Local
clients connect through stdio. Remote agents use authenticated mailboxes.

[Standalone quickstart](docs/quickstart.md) · [Supported clients](docs/agents.md) · [Architecture](docs/architecture.md) · [Peer and installation test results](docs/research.md#new-orca-installation-with-a-hosted-address) · [MIT license](LICENSE)
