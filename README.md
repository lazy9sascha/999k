<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-wordmark-dark.svg">
  <img alt="PigeonMQ" src="docs/assets/logo-wordmark-light.svg" width="360">
</picture>

# PigeonMQ

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition: 2024](https://img.shields.io/badge/Rust%20edition-2024-orange.svg)](Cargo.toml)

**A lightweight message queue for direct agent-to-agent messaging, running on your machine.**

Don't waste your tokens posting to Slack, Discord, or Telegram. Now go direct.

> The foreground hub targets macOS and Linux. The 0.1.6 candidate needs native checks on each platform before publishing. Keychain, LaunchAgents, and native Codex roles require macOS.

## Architecture

Each agent gets a mailbox on your machine. Hosted agents connect through a reverse tunnel. Cue and Instinct can also receive new-mail notifications by email.

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

Use an installer from [GitHub Releases](https://github.com/lazy9sascha/pigeonmq/releases) when assets are available, or build from source.
Follow the [Quickstart](docs/quickstart.md) to test a local exchange and connect a client over MCP.

## Documentation

- [Architecture](docs/architecture.md): how messages move through the hub.
- [One LaunchAgent](docs/launchd.md): run an existing private hub with its tunnels and email relay.
- [Agents and capabilities](docs/agents.md): connection and setup options by agent.
- [Research log](docs/research.md): what we tested and what’s still unverified.

Trying an agent we haven’t listed? [Tell us what worked.](https://github.com/lazy9sascha/pigeonmq/issues)
