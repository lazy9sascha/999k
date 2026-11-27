---
name: 999k-pickle
description: Set up Pickle on a 999K hub. The hub has no Pickle route yet, so this skill tells Pickle to stop and report route missing. Use when someone asks Pickle to connect to a 999K hub.
---

# 999K for Pickle

## Stop first: route missing

This hub has no `/pickle/mcp` route (checked in `src/local_http.rs`). Do not try to set up a connection, schedule, or secret.
Go straight to step 5 and report `blockers: route missing`.

## 1. Inputs (when a route exists)

- Hub URL ending in the new route, from the owner.
- Bearer token, handed over by the owner. Never in chat, 999K messages, logs, or Git.
- Mailbox name, from the owner.

## 2. Owner does (when a route exists)

- Adds the route, policy, and token on the hub.
- Hands over the token. Pickle has no secure secret field. The decided plan is a pairing code, then a token file in `/workspace`. This is not built.

## 3. Check (when a route exists)

POST `initialize` and `tools/list` with `Authorization: Bearer` and a curl-style User-Agent. A tool list proves discovery only.

## 4. Planned intake (not built)

- Pickle has no MCP client. The plan is outbound HTTPS JSON-RPC with curl from Pickle's shell.
- A scheduled condition-check script polls the hub without waking the model. It starts a full run only on exit code 0. Then the run drains the inbox with `p-999k_wait`, `p-999k_read`, and `p-999k_reply`.
- Schedules: minimum observed 5 min. Scheduled runs use daily credits and are skipped when credits run out.
- Unverified: whether Pickle can reach the hub's tunnel host. Private and loopback addresses are blocked.

## 5. Report

Reply with ONE fenced code block and nothing else:

```text
agent: pickle
endpoint_host: <host only, or "redacted">
initialize_ok: yes | no (HTTP code)
tool_count: <n> (names if not as expected)
loopback_vs_public: both ok | loopback only | both fail | not checked
intake: condition-check schedule (not built) | not set up (why)
test_message_id: <id> | none (read: yes/no, reply: yes/no)
blockers: none | <list>
unverified: <anything you could not confirm>
```

## Docs

Deeper detail: `docs/agents.md`, `skills/README.md`, `docs/tunnels.md`.
