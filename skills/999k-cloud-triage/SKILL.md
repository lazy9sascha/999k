---
name: 999k-cloud-triage
description: Agent triage: classify an unfamiliar AI agent or service and pick its adapter type before connecting it to 999K. Use when an agent has no skill under skills/999k-<agent>/.
---

# Agent triage

Follow [agent triage](../../docs/architecture.md#agent-triage): **triage prompt → capability row → triage rules → adapter type**.
Send the [agent triage prompt](../../docs/agents.md#agent-triage-prompt) to the service once.
Record its answer as a capability row.
Then apply the [triage rules](../../docs/architecture.md#triage-rules).

If the agent is already known, use its connect skill instead: [Grok](../999k-grok/SKILL.md), [Muse](../999k-muse/SKILL.md), [Dot](../999k-dot/SKILL.md), [Codex](../999k-codex/SKILL.md), [Pickle](../999k-pickle/SKILL.md), or [Vooy](../999k-vooy/SKILL.md).

Identify how the service can act as a client of 999K. A service that exposes its own MCP server does not necessarily accept a custom MCP server.

1. Record the exact product, app or web surface, and version if known. Use the vendor's documentation or a settings screen. Treat a chat answer as a lead, not proof.
2. Identify who initiates the connection and where that client runs. Record the transport, authentication, and setup path. Note whether the operator must use a settings screen or whether the app accepts setup by prompt.
3. Choose the path with the strongest evidence from the table. If you lack evidence, record `unknown`.

| Path | What to look for | `adapter_path` |
| --- | --- | --- |
| Remote MCP client | A custom HTTPS MCP endpoint and a supported authentication method, such as Bearer or OAuth. | `remote_mcp` |
| Local MCP client | An app host that can use stdio or a loopback endpoint on the queue Mac. | `local_mcp` |
| App bridge or API | An official host, API, or CLI that accepts requests in an explicitly selected conversation or session. | `app_bridge` |
| No supported path | The vendor or actual settings show that none of these inputs is available. | `none` |
| Unknown | The available evidence does not identify an input. | `unknown` |

4. Record connection evidence separately: `candidate`, `configured`, `tools_listed`, `message_read`, or `reply_ack_verified`. Use only the highest step with evidence. Record wake support separately. Do not infer it from MCP.
5. Give the next smallest step. For a remote client, make sure that the authentication field is known. Then do an approved read of one synthetic message. For a bridge, make sure that its host and session binding are known before any delivery.
6. Write the record to `<home>/triage/<agent>.json` (the default home is `.local`). Make sure that the file matches [the triage schema](../../schemas/triage.schema.json). Record the requested operations and `send_to` peers in that file. The file must not contain tokens or live hosts.

Triage does not change settings, register a connector, or send credentials. Do not add a candidate to the active adapter list.

Give a short human-readable summary of the same fields. See [the Vooy example file](../../schemas/examples/vooy.json). See also the [Vooy research entry](../../docs/research.md#vooy-01-custom-mcp-candidate).

[Triage file to init](../../docs/architecture.md#triage-file-to-init) explains what an approved local-only staging run can use. A remote candidate is not staged or connected.
