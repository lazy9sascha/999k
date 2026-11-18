---
name: pigeonmq-vooy
description: Assess Vooy for PigeonMQ. A synthetic loopback route can be staged. The real Vooy app is not verified or configured.
---

# PigeonMQ for Vooy

## Stop first: the real connection is unverified

A synthetic `/vooy/mcp` route can be served in a dedicated [local test home](../../docs/configuration.md#stage-local-mailboxes). It does not receive calls from the Vooy app. The checked-in triage candidate cannot stage a route because transport and Bearer support are unknown. Do not configure the app. Do not send a credential. Do not open a tunnel for this test. Report `blockers: remote connection unverified`.

## 1. Inputs (after the owner approves remote verification)

- Hub URL ending in the new route, from the owner.
- Bearer token, handed over by the owner. Never in chat, Pigeon, logs, or Git.
- Mailbox name, from the owner.

## 2. Owner does (after the owner approves remote verification)

- Adds the route, policy, and token on the hub.
- In Vooy: turn on Labs (My > Settings > Labs > custom MCP).
  Add the server on the website service connection page.
  Tick the trust box.
  Switch tools on one by one.
- Enters the token in the header field. Unverified: whether the header name can be `Authorization`, whether `Bearer ` is added, and whether the field is masked.

## 3. Check (after the owner approves remote verification)

POST `initialize` and `tools/list` with `Authorization: Bearer` and a curl-style User-Agent. A tool list proves discovery only.

## 4. Planned intake (not built)

- Pull only through MCP. Vooy has no shell and no raw HTTPS POST with headers.
- A Vooy schedule (minimum 15 min, at most 5 active) would start a run. That run would drain the inbox with `pigeonmq_wait`, `pigeonmq_read`, and `pigeonmq_reply`.
- Unverified: whether MCP tools run inside scheduled runs, the transport, and whether Vooy accepts the hub's tunnel host.

## 5. Report

Reply with ONE fenced code block and nothing else:

```text
agent: vooy
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tool_count: <n> (names if not as expected)
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: Vooy schedule (not built) | not set up (why)
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Docs

Deeper detail: `docs/agents.md`, `docs/setup-skills.md`, `docs/tunnels.md`.
