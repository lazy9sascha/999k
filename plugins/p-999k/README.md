# P-999K

Ask an existing 999K peer from your Orca project chat. The matching reply returns to that chat when the peer sends it.

**Catalog summary.** Ask an existing 999K peer from an Orca project chat. The matching reply returns to that chat.

This is plugin **P-999K** 0.2.1. The catalog ID is `lazy9sascha.p-999k`. It needs Orca 1.4.268 or newer and the native `999k` CLI. Use the CLI from this checkout for a fresh installation. The published 0.2.0 CLI requires an existing hub, a private stdio socket and an authorized sender.

Catalog publication is pending. A fresh Orca profile has not yet completed owner approval and the first linked reply. Do not treat those results as done.

If you installed plugin 0.2.0 as `local.radio999k`, review this plugin and approve its trusted worker. It uses `workspace:read` and `terminal:send`. Orca cannot approve the plugin or create the first bot for you.

## Setup

Open a local workspace and select **P-999K** in the right sidebar. Choose **Initial setup** and open the setup terminal. On a fresh Mac home the CLI starts the managed loopback hub and its private stdio socket. It creates a local sender with zero external peers. On an existing hub it lists the supported peers the sender can address.

For chat-based setup, choose **Set up a management bot**. Create **P-999K** once in **Bots → New bot** and give it the purpose shown in the panel. Ask in its Home Chat, for example, **“Connect Muse.”** It can run the same command as selecting Muse in the panel:

```sh
"$HOME/.local/bin/999k" init --orca --orca-peer muse
```

For a selected peer, choose **I’ll do it myself** to open the terminal and follow the steps yourself. **Ask my assistant to do it** shows a request you can copy to an assistant that already has that capability. Both paths use the same CLI and require any native approvals and private inputs. Review the relay approval in Orca. If setup prints a resume command, run it after approval. Paste the peer-specific setup text into Muse’s own chat and configure its reachable hub address and private credential. A linked reply verifies that Muse can receive and answer a message. Use the P-999K bot for connection setup and your regular project chat for questions to peers.

For a remote client with pairing support, select its name and choose **Get a setup code**. Paste the terminal’s offer into that peer’s private chat. The code expires after ten minutes and can be used once. The peer stores its credential locally. [Pair a remote client](../../docs/pairing.md)

## One question

Stay in the project chat and ask the project agent to consult one peer. For example: “Ask Muse for a shorter title for the release notes.”

The project agent asks the Muse relay bot. The intended connector is local stdio from that bot to the hub. A fresh profile has not completed that round trip for Dot, Muse, GrokBot, Cue and Instinct together. 999K 0.2.0 is the shipped CLI and hub. Earlier live replies used the Cloud Codex mailbox. Synthetic stdio used a temporary queue. **Queued** means the hub accepted the request. It is not the answer.

Muse reads the request from its existing mailbox. Muse polls `/muse/mcp` over HTTPS through the hub’s tunnel. This plugin does not open that tunnel. Dot, Grok Bot, Cue, and Instinct keep their own existing routes. Cue and Instinct can also get an email that a message is waiting. They still read and reply through MCP.

The relay bot can check again later. Only a reply linked to the same request is the answer. The answer can arrive later. The bot returns it to the project question that asked.

The peer’s Home Chat shows that bot’s tool calls and its own updates. It is a work record. It is not a second copy of the project chat.

## Checks

From the folder that contains `orca-plugin.json`:

```sh
orca plugin validate .
orca plugin check .
```

In the [999K development repository](https://github.com/lazy9sascha/p-999k), run `orca plugin test plugins/p-999k`.
