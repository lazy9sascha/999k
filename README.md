# P-999K

<img alt="A worn 999K field radio with a bent antenna" src="docs/assets/999k-field-radio-ink.png" width="240">

**Ask the agents you already use for help from your Orca project chat.**

Once you connect an agent, ask about it from the Orca project chat:

> Ask my connected agent to review the install instructions. Which step would a first-time user get stuck on?

The relay sends your question to the selected agent. Its answer returns to the project chat where you asked.

## Connect your first agent

On the Mac where you use Orca, [install the 0.2.2 CLI and Orca plugin](docs/quickstart.md#connect-orca-to-your-existing-peers). You’ll need an Apple Silicon Mac and Orca 1.4.268 or later.

1. Open **P-999K → Initial setup** in a local Orca workspace. It prepares the hub on your Mac. You choose which agents to connect.
2. Select an agent you already use. Choose **I’ll do it myself** for the guided setup, or copy the request to an assistant with Computer Use. Approve the relay in Orca and copy the peer’s setup instructions into its existing conversation.
3. Return to your project chat and ask your agent to consult that peer. **Queued** means your Mac has stored the question. The relay can check the same request again after the first wait ends.

You can connect **Dot, Muse, GrokBot, Cue and Instinct** individually. The hub keeps a separate mailbox for each peer. Cue can receive an ID-only email notice. Instinct can receive a manual iMessage notice. Both send their answers through the hub. Dot can connect through an operator-protected OAuth endpoint. [Connection details for each agent](docs/quickstart.md#prepare-the-selected-peers-transport)

## Use the hub without Orca

The `999k` CLI runs a self-hosted message queue and MCP hub. Local clients connect through stdio. Remote agents reach their own mailbox through an authenticated connection. A reply stays linked to the original question even if the sender checks back later.

[Standalone quickstart](docs/quickstart.md) · [Supported clients](docs/agents.md) · [Architecture](docs/architecture.md) · [Peer and installation test results](docs/research.md#orca-onboarding-and-peer-returns-2026-11-25) · [MIT license](LICENSE)
