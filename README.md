<img alt="Ink drawing of the 999K field radio with a bent antenna and coiled handset cord" src="docs/assets/999k-field-radio-ink.png" width="280">

# 999K

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition: 2024](https://img.shields.io/badge/Rust%20edition-2024-orange.svg)](Cargo.toml)

**A field radio for your agents.**

999K (pronounced “nine nine nine K”) is a message queue on your machine. Your agents can ask each other for help and retrieve replies, even if they come back later.

Orca is one way to use the queue. The same hub also serves local clients and hosted agents.

## Ask from your project chat

Stay in your current Orca project chat. Ask that agent to consult a peer you already run.

> Ask Muse for a shorter title for the release notes.

Your project agent sends the request to the Muse bot. You can continue in the same chat while you wait. When the bot retrieves Muse’s reply, it can return the answer to your original question. If the first check is empty, the bot may need to check again.

Open the Muse bot Home Chat to see its tool calls and its own updates. It shows the relay’s part of the exchange.

**Queued** means the hub accepted the request. The answer arrives in a separate message linked to the original question.

[Connect your agents to Orca](docs/quickstart.md#connect-orca-to-your-existing-peers)

## How an Orca chat reaches Muse

The Orca plugin opens setup on the Mac that runs your 999K hub. Setup connects an Orca relay bot to each peer you configure. Here is the path for one question to Muse.

```mermaid
flowchart LR
    subgraph Orca["Orca on your Mac"]
        Chat["Project chat agent"]
        Bot["Muse relay bot"]
        Chat -->|Ask| Bot
    end
    Bot -->|Send and check via stdio| Hub["999K hub on 127.0.0.1"]
    Muse["Existing Muse"] -->|MCP over HTTPS + Bearer| Tunnel["Public HTTPS tunnel"]
    Tunnel --> Hub
```

The hub binds to loopback. Muse polls its MCP route through the hub’s HTTPS tunnel. Cue can receive an ID-only email. Instinct used an ID-only iMessage in its existing conversation. Both read the request and answer through MCP.

On a configured Mac, Dot, GrokBot, Cue, Instinct and Muse each answered through their own relay. Cue and Instinct received manual notices. Muse’s first wait was empty. The project agent retrieved the matching replies. The first install from public artifacts through a linked reply is still open.

Local clients can use the same hub directly through stdio. [Hub architecture](docs/architecture.md)

## Supported agents

| Agent | Mailbox access |
| --- | --- |
| Grok Bot | MCP with webhook notifications |
| Muse | MCP polling |
| Dot (ChatGPT) | MCP Events over a Secure MCP Tunnel |
| Codex Cloud | MCP with optional Events over a Secure MCP Tunnel |
| Cue | MCP with optional email notifications |
| Instinct | MCP from its private runtime, with a manual iMessage arrival notice |

For local Codex sessions, see the [companion project](https://github.com/lazy9sascha/pigeonmq-local).

## Quickstart

**For Orca**, use the [candidate setup instructions](docs/quickstart.md#connect-orca-to-your-existing-peers). The panel needs a CLI that supports peer selection and pairing. The published 0.2.0 CLI lacks those commands.

For standalone 999K 0.2.0, use an installer from [GitHub Releases](https://github.com/lazy9sascha/p-999k/releases) when you have access to the assets. It installs the two executables in `$HOME/.local/bin`.

```sh
version=0.2.0
curl -q --proto '=https' -fLsS -o 999k-install.sh \
  "https://github.com/lazy9sascha/p-999k/releases/download/$version/install.sh" &&
sh ./999k-install.sh
```

```sh
export PATH="$HOME/.local/bin:$PATH"
999k --version
```

For a local mailbox, continue with the [standalone quickstart](docs/quickstart.md#local-mcp-on-macos).

A reverse tunnel is optional. Use it only for a remote agent.

## Platform limits

- The 0.2.0 binaries target Apple Silicon Macs and Linux on x86-64 and ARM64.
- Managed setup, Keychain, and native Codex roles need macOS.
- The Orca plugin needs Orca 1.4.268 or newer. Review the plugin permissions and each bot creation request in Orca.
- Keep an existing hub on its current path. Read the Quickstart before you add stdio to a home that already runs.
- Release archives exist only for targets published on [GitHub Releases](https://github.com/lazy9sascha/p-999k/releases). Use the Quickstart to build 999K when that list has no archive for your machine.

## Documentation

- [Architecture](docs/architecture.md): how messages move through the hub.
- [One LaunchAgent](docs/launchd.md): run an existing private hub with its tunnels and email relay.
- [Agents and capabilities](docs/agents.md): connection and setup options by agent.
- [Research log](docs/research.md): what we tested and what is still unverified.
- [Contributing](CONTRIBUTING.md): how to test a change.

Trying an agent we have not listed? [Tell us what worked.](https://github.com/lazy9sascha/p-999k/issues)

## License

The [MIT License](LICENSE) covers 999K.
