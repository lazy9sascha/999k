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

## The hub

Each agent has a mailbox on your machine. Local clients use stdio on that machine. Hosted agents can use a reverse tunnel. Cue and Instinct can also receive an email when a message arrives.

```mermaid
flowchart LR
    subgraph HOSTED["Hosted agents"]
        G["Grok Bot"]
        M["Muse"]
        D["Dot"]
        HX["Other hosted agents"]
        CE["Cue · Instinct"]
    end

    subgraph MAC["Your machine"]
        S["Local stdio"]
        T["Optional reverse tunnel<br/>Email relay"]
        Q["999K<br/>Message queue"]

        subgraph LOCAL["Local agents"]
            H["Hermes"]
            C["Codex"]
            O["OpenCode"]
            LX["Other local agents"]
        end
    end

    S <--> Q
    G & M & D & HX <--> T
    T <--> Q
    S <--> C & O & H & LX
    CE -->|Email relay| T
```

## Supported agents

| Agent | Mailbox access |
| --- | --- |
| Grok Bot | MCP with webhook notifications |
| Muse | MCP polling |
| Dot (ChatGPT) | MCP Events over a Secure MCP Tunnel |
| Codex Cloud | MCP with optional Events over a Secure MCP Tunnel |
| Cue | MCP with optional email notifications |
| Instinct | MCP with optional email notifications |

For local Codex sessions, see the [companion project](https://github.com/lazy9sascha/pigeonmq-local).

## Quickstart

The public command is `999k`. Use an installer from [GitHub Releases](https://github.com/lazy9sascha/999k/releases) when the 0.2.0 assets are available. The installer places `999k` and `999k-dot-resolve` in `$HOME/.local/bin`. It does not start a service or open a tunnel.

```sh
version=0.2.0
curl -q --proto '=https' -fLsS -o 999k-install.sh \
  "https://github.com/lazy9sascha/999k/releases/download/$version/install.sh" &&
sh ./999k-install.sh
```

```sh
export PATH="$HOME/.local/bin:$PATH"
999k --version
```

Then follow the [Quickstart](docs/quickstart.md).

1. Prepare local stdio on the Mac that runs the hub.
2. Approve the Orca plugin. In an empty Orca profile, create the first bot.
3. Send one request from the project chat.
4. Treat **Queued** as acceptance. Wait for the matching peer reply.

A reverse tunnel is optional. Use it only for a remote agent.

## Platform limits

- The foreground hub runs on macOS and Linux.
- Managed setup, Keychain, and native Codex roles need macOS.
- The Orca plugin needs Orca 1.4.268 or newer. Review the plugin permissions and each bot creation request in Orca.
- Keep an existing hub on its current path. Read the Quickstart before you add stdio to a home that already runs.
- Release archives exist only for targets published on [GitHub Releases](https://github.com/lazy9sascha/999k/releases). Use the Quickstart to build 999K when that list has no archive for your machine.

## Documentation

- [Architecture](docs/architecture.md): how messages move through the hub.
- [One LaunchAgent](docs/launchd.md): run an existing private hub with its tunnels and email relay.
- [Agents and capabilities](docs/agents.md): connection and setup options by agent.
- [Research log](docs/research.md): what we tested and what is still unverified.
- [Contributing](CONTRIBUTING.md): how to test a change.

Trying an agent we have not listed? [Tell us what worked.](https://github.com/lazy9sascha/999k/issues)

## License

The [MIT License](LICENSE) covers 999K.
