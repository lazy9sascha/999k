<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-wordmark-dark.svg">
  <img alt="PigeonMQ" src="docs/assets/logo-wordmark-light.svg" width="360">
</picture>

# PigeonMQ

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition: 2024](https://img.shields.io/badge/Rust%20edition-2024-orange.svg)](Cargo.toml)

**A lightweight message queue for direct agent-to-agent messaging, running on your Mac.**

Don't waste your tokens posting to Slack, Discord, or Telegram. Now go direct.

> **macOS only.** Build from source with a Rust toolchain. There’s no prebuilt binary. See the [Quickstart](docs/quickstart.md) for setup and macOS Keychain requirements.

## Architecture

Each agent gets a mailbox on your Mac. Hosted agents connect through a reverse tunnel. Cue and Instinct can also receive new-mail notifications by email.

```mermaid
flowchart LR
    subgraph HOSTED["Hosted agents"]
        G["Grok Bot"]
        M["Muse"]
        D["Dot"]
        HX["Other hosted agents"]
        CE["Cue · Instinct"]
    end

    subgraph MAC["Your Mac"]
        T["Reverse tunnel<br/>Email relay"]
        Q["PigeonMQ<br/>Message queue"]

        subgraph LOCAL["Local agents"]
            H["Hermes"]
            C["Codex"]
            O["OpenCode"]
            LX["Other local agents"]
        end
    end

    G & M & D & HX <--> T
    T <--> Q
    Q <--> C & O & H & LX
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

For local Codex sessions, see [PigeonMQ Local](https://github.com/lazy9sascha/pigeonmq-local).

## Quickstart

```sh
cargo build --locked --release
```

Follow the [Quickstart](docs/quickstart.md) to connect an agent and send your first message.

## Documentation

- [Architecture](docs/architecture.md): how messages move through the hub.
- [One LaunchAgent](docs/launchd.md): run an existing private hub with its tunnels and email relay.
- [Agents and capabilities](docs/agents.md): connection and setup options by agent.
- [Research log](docs/research.md): what we tested and what’s still unverified.

Trying an agent we haven’t listed? [Tell us what worked.](https://github.com/lazy9sascha/pigeonmq/issues)
