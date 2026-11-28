# P-999K

**Ask Dot (ChatGPT), Muse, GrokBot, Cue, or Instinct (Messages) from your Orca project chat.**

Ask Muse to turn a rollout plan into a checklist and GrokBot to challenge
the rollback steps. Each agent works in its own app. P-999K brings each
answer back to your project chat and links it to its question. If the first
wait ends early, the relay can check the original request again.

[Watch the Orca demo](https://lazy9sascha.github.io/p-999k/) · [Install from Orca Community](docs/orca.md) · [Try a prompt and check the reply](docs/orca.md#verify-the-first-reply)

<img alt="A worn 999K field radio with a bent antenna" src="docs/assets/999k-field-radio-ink.png" width="200">

## What you need

Use an Apple Silicon Mac with Orca 1.4.268 or later.

P-999K is experimental. The hosted service resets tunnel addresses on a
five-day schedule. Your queue stays on your Mac. For a stable address, use
the same hub with your own HTTPS domain. Check a reply on the new address
before retiring the old connection.
[Switch to your own domain](docs/orca.md#switch-to-your-own-domain)

## Try A2A with an agent you run

Use the experimental Agent-to-Agent (A2A) adapter to hand off a text task,
such as asking an internal review agent for findings on a migration plan.
Configure one A2A 1.0 JSON-RPC endpoint that can return a final text answer
from one request. If the receiver restarts after it saves a task ID, it can
look up the same task. Handle any follow-up input or sign-in at the agent.

Build from `main` to try the adapter. [Configure the A2A receiver](docs/configuration.md#a2a-receiver)

## Use the hub without Orca

Run the `999k` CLI to keep a message queue and MCP hub on your Mac. Local
clients connect through stdio. Remote agents use authenticated mailboxes.

[Standalone quickstart](docs/quickstart.md) · [Supported clients](docs/agents.md) · [Architecture](docs/architecture.md) · [Peer and installation test results](docs/research.md#new-orca-installation-with-a-hosted-address) · [MIT license](LICENSE)
