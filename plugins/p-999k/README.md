# P-999K

Ask an existing 999K peer from your Orca project chat. The relay retrieves the linked answer for that chat. If its first check is empty, ask it to check the same request again.

**Catalog summary.** Ask one existing peer from your project chat. The relay retrieves the linked answer. The first check can be empty.

P-999K 0.2.1 needs Orca 1.4.268 or newer. Its catalog ID is `lazy9sascha.p-999k`. The panel runs `$HOME/.local/bin/999k`. Check that this executable lists `--orca-peer` in `init --help` and accepts `pair --help`. Use native CLI 0.2.1. [Install the CLI and plugin](../../docs/quickstart.md#connect-orca-to-your-existing-peers).

The release archive installs through Orca’s local plugin flow. Catalog availability is tracked separately.

Orca displays the trusted worker’s permissions before installation. `workspace:read` lets setup find the active local workspace. `terminal:send` lets it open a terminal there and run the selected CLI command. Review the exact files and access before approving. To stop an installed plugin, run `orca plugin disable lazy9sascha.p-999k`. To remove its files and approval, run `orca plugin uninstall lazy9sascha.p-999k`. Peer credentials and the hub have their own lifecycle.

## Setup

Open a local workspace and select **P-999K** in the right sidebar. Choose **Initial setup** and open the setup terminal. On a fresh Mac home the CLI starts the managed loopback hub and its private stdio socket. It creates a local sender with zero external peers. On an existing hub it lists the supported peers the sender can address.

For chat-based setup, choose **Set up a management bot**. Create **P-999K** once in **Bots → New bot** and give it the purpose shown in the panel. Ask in its Home Chat, for example, **“Connect Muse.”** It can run the same command as selecting Muse in the panel:

```sh
"$HOME/.local/bin/999k" init --orca --orca-peer muse
```

Choose **I’ll do it myself** for instructions you can follow in the terminal and the peer app. **Ask my assistant to do it** copies a request for an assistant with Computer Use. Approve the relay in Orca when prompted, then run any resume command on the hub Mac. Give the selected peer only its own setup instructions. Use your regular project chat for questions.

For Muse, GrokBot, Cue or Instinct, publish the selected peer’s pairing and MCP routes before you choose **Get a setup code**. Enter the ten-minute code in the peer’s private setup flow. Instinct can present a secure form. Dot uses a private ChatGPT tunnel. [Pair a remote client](../../docs/pairing.md)

## One question

Stay in the project chat and ask the project agent to consult one peer. For example: “Ask Muse for a shorter title for the release notes.”

The project agent asks the Muse relay bot, which sends through local stdio. **Queued** means the hub accepted the request. A prepared Mac completed this exchange with all five peers. Cue and Instinct received manual notices. Muse’s first wait was empty. The fresh-profile installation and first linked reply remain open.

Muse polls its existing mailbox over HTTPS through the hub’s tunnel. Cue received a manual ID-only email and Instinct received a manual ID-only iMessage. Both read and replied through MCP.

The relay bot can check again later. Only a reply linked to the same request is the answer. The answer can arrive later. The bot returns it to the project question that asked.

The peer’s Home Chat shows that bot’s tool calls and its own updates. It is a work record. It is not a second copy of the project chat.

## Checks

From the folder that contains `orca-plugin.json`:

```sh
orca plugin validate .
orca plugin check .
```

In the [999K development repository](https://github.com/lazy9sascha/p-999k), run `orca plugin test plugins/p-999k`.
