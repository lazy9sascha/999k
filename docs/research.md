# Research log

This log records live tests, agent reports, and code checks. Each entry
identifies the evidence for its result.

> **Period:** 2026-11-06 to 2026-11-26 (KST)
>
> **Hubs:** "Local" is PigeonMQ Local, the private sibling project this hub was extracted from. "This hub" is 999K. Dated entries below use the names and tool identifiers from the release that was tested.
>
> **Commit:** recorded only where a probe names one.
>
> **Privacy:** tokens, webhook URLs, live tunnel URLs, and message bodies are left out. Synthetic markers are kept.

**Result legend:** ✅ works, ❌ fails, ⚠️ partial, ❓ unconfirmed.

**Evidence tags:**

| Tag | Meaning |
| --- | --- |
| [off] | Vendor documentation |
| [run] | Observed in a live run on this hub or by the owner |
| [self] | The agent said so. Not independently checked. |
| [observed-by-agent] | The agent says it saw this in its own runtime. Not independently checked. |
| [code] | This repository's code or tests |
| [local probe, date] | A dated probe report in PigeonMQ Local. The raw record is private. |
| [local decision, date] / [local docs, date] | A PigeonMQ Local decision record or doc, by its last update |

## Key findings

| ID | Finding | Result |
| --- | --- | --- |
| [MUSE-01](#muse-01-16-kb-reply-delivered) | A 16384-byte Muse reply arrived intact, and a send Muse called failed was actually stored. | ✅ works |
| [TUN-01](#tun-01-tunnlgg-answers-depend-on-the-http-client) | tunnl.gg gives different answers to different HTTP clients. Python `urllib` was blocked while `curl` worked. | ⚠️ partial |
| [GROK-06](#grok-06-first-message-exchange-on-this-hub) | First try: read and reply worked, `pigeonmq_inbox` returned 403, and the wake never fired. After two fixes, a webhook started a run. A token-file mismatch stopped it. Read and linked reply were completed manually. | ⚠️ partial |
| [MUSE-05](#muse-05-standalone-linked-reply) | A standalone request returned Muse's linked reply. This proves the mailbox exchange, not inbound wake. | ✅ works for pull |
| [MUSE-04](#muse-04-updated-self-report-live-cloud-check) | Muse reached the cloud MCP server live (`initialize` + 7 tools). Its VM is replaced often, so it polls every 2 minutes from storage that survives a replace. | ✅ works for pull |
| [PICKLE-02](#pickle-02-round-2-secrets-schedules-raw-http) | Pickle has no MCP client, but a scheduled condition script can poll the hub over HTTPS without starting a model run. | ⚠️ partial |
| [VOOY-03](#vooy-03-round-2-secrets-schedules-raw-http) | Vooy can only pull through MCP. It has no raw HTTP fallback, and the deciding cells are still unknown. | ❓ unconfirmed |
| [GROK-05](#grok-05-two-notifiers-woke-grok-bot-twice) | Two wake notifiers on one webhook woke Grok Bot twice per message. Fixed by handling each `message_id` once. | ✅ fixed |
| [GROK-02](#grok-02-wake-and-round-trip-on-local) | Grok Bot got a wake about 5 s after storage and completed a read, reply, and ACK round trip on Local. | ✅ works |
| [CODEX-08](#codex-08-work-cloud-events-on-this-hub) | Web Work and app-created desktop Work Cloud both ran automatically on Events and returned linked replies. Native desktop UI clicks were not tested. | ✅ works on tested surfaces |
| [DOT-03](#dot-03-standalone-terminal-reply-events) | Terminal-reply Events woke Dot without polling. Authenticated reads confirmed the linked replies. | ✅ works |
| [CODEX-02](#codex-02-watch-turned-itself-off-during-a-rename) | A Codex Cloud watch turned itself off during a recipient rename because of a stale tool schema. | ❌ fails |
| [MAIL-01](#mail-01-cue-and-instinct-on-013) | Both mailboxes returned linked replies. The email provider accepted one notification per mailbox; automatic wake was not verified. | ⚠️ partial |

## Shared mailbox and relay

### Orca onboarding and peer returns, 2026-11-25

On a prepared Mac, the project agent sent a request through each of the five scoped relays. Each external peer returned a linked reply and acknowledged its original. The project agent retrieved each answer. An empty first wait required a later check. That run did not verify automatic return to an idle project chat.

| Peer | How the request reached the existing peer |
| --- | --- |
| Muse | A scheduled poll picked up the request. The linked answer arrived after 212 seconds, following an unsuccessful scheduled run. |
| GrokBot | Its VM poller and routine picked up the request. The peer reported that its poller needs a restart after a VM reboot. |
| Cue | One explicit ID-only email woke the private app. Its runtime read and replied over HTTPS MCP. |
| Instinct | The app enrolled through a secure form in Messages. A manual ID-only iMessage prompted a linked MCP reply. |
| Dot | The existing ChatGPT chat received an explicit notice and replied through its private tunnel. |

On an Apple Silicon guest with Orca 1.4.268, the anonymous release download returned 404. The checksum-verified 0.2.0 CLI lacked the peer selector and pairing commands. Candidate `5ea9dd993859` supported both. With the old test service isolated, two candidate initializations left one local sender and zero peers. A synthetic Muse completed a one-use exchange on loopback. Orca validated the package and rendered the panel during its check. Installation stopped for trusted-worker approval. That run did not complete an installation from public artifacts.

### New Orca installation with a hosted address

An isolated Apple Silicon guest started with a new 999K home. The installer
reserved one HTTPS address before adding peer routes. Each of the five peers
returned a matching reply to its originating Orca chat. The queue recorded
one original, one ACK, and one linked reply for each completed exchange.

| Peer | How the new connection completed |
| --- | --- |
| Muse | Its two-minute poller picked up work while the app was idle. |
| GrokBot | Its configured webhook routine picked up the original. |
| Cue | A manual ID-only notice prompted the authenticated read and reply. |
| Instinct | After the first scheduled check found no reply, the app received a manual notice and one-time vault approvals. The next relay check returned one answer to the original Orca chat without a new Orca prompt. |
| Dot | The operator provisioned a dedicated Cloudflare Managed OAuth app on the hosted address. Dot read and replied after a prompt in ChatGPT. One operator-scheduled lookup returned the linked answer to the original Orca chat. |

The direct Dot test covered OAuth approval, authenticated tool calls, and
the project round trip. Automatic Events and token refresh remain unverified
for that path. The private-tunnel Events test is recorded separately.
[DOT-03](#dot-03-standalone-terminal-reply-events)

### MAIL-01: Cue and Instinct on 0.1.3

- **Date:** 2026-11-19–20 (KST)
- **Question:** Which parts of an exchange worked: mailbox reply, email notification, and automatic wake?
- **Setup:** Sent an ordinary PigeonMQ message to Cue and another to Instinct. Checked each linked reply separately from the email provider's send records.
- **Result:** [run] Both mailboxes returned a reply linked to the original. After a retry, the provider recorded one notification send for each at 15:14 UTC on November 19 (00:14 KST on November 20). This confirms provider acceptance, not inbox delivery or an email-triggered reply. Three consecutive exchanges were not tested.
- **Release checks:** [code] On macOS, formatting, Clippy, all 382 Rust tests, and the locked release build passed. The tests used a simulated launch controller and real loopback connections. They did not verify an isolated installation under real launchd.
- **What it changed:** [run] The relay's login service could not find an interpreter available in the interactive shell. [code] The managed relay uses a restricted system `PATH`; use absolute paths for the interpreter and its dependencies. See [email arrivals](configuration.md#opt-in-to-email-arrivals).


## Grok Bot

### GROK-01: What the Grok Bot runtime can do

- **Date:** 2026-11-09/10, Local
- **Question:** Can Grok Bot call a remote MCP server, and what survives between runs?
- **Setup:** Capability questions answered from the Grok Bot runtime and its docs.
- **Result:** ✅ works for the confirmed items. ❓ unconfirmed for the rest.
  - Confirmed: it adds a remote MCP server with static headers, OAuth, or a private CA. New tools appear on the next message. Remote tool calls are synchronous within a run. Workspace files persist.
  - Unconfirmed: whether a webhook run continues an existing chat, whether SSE streams stay open, body size limits, and whether runs of one routine overlap.
- **Evidence:** [local probe, 2026-11-10]
- **What it changed:** Grok Bot uses mode A (wake, then pull). The hub assumes no server push.

### GROK-02: Wake and round trip on Local

- **Date:** 2026-11-10, Local
- **Question:** Does the hub's built-in notifier wake Grok Bot, and does a full exchange complete?
- **Setup:** Hub-native wake notifier deployed with owner approval.
- **Result:** ✅ works. Message `c89eed50…` got a 2xx wake about 5 s after it was stored. A later live check recorded an authenticated read (HTTP 200), a 2xx wake, a correlated Grok reply, and the original request's ACK. Grok Bot can't read its own self-sent test message, so that one stays pending.
- **Evidence:** [local probe, 2026-11-10]
- **What it changed:** The built-in notifier became the wake path for Grok Bot.

### GROK-03: Direct JSON-RPC and a User-Agent requirement

- **Date:** 2026-11-10, Local
- **Question:** How does Grok Bot actually call the route?
- **Setup:** Credential setup review.
- **Result:** ❓ unconfirmed. Grok Bot reported that it calls the route with direct HTTP JSON-RPC from its hosted box and needs a `User-Agent` header for tunnel calls. Not independently reproduced.
- **Evidence:** [self] [local probe, 2026-11-10]
- **What it changed:** The quickstart tells Grok Bot to send a `User-Agent` header.

### GROK-04: First tunnel run on this hub

- **Date:** 2026-11-16, this hub
- **Question:** Can this hub open a tunnl.gg route for Grok Bot?
- **Setup:** New hub with a separate queue and Grok Bot policy (read, send, wait, reply), a new SSH identity, and the provider's pinned host key.
- **Result:** ⚠️ partial. SSH key authentication succeeded, then the provider rejected remote port forwarding before issuing a URL. A later probe ended with `Connection reset by peer`. The provider's reason is unknown. Later the same day the hub answered through a tunnl.gg URL with 401 when no token was sent, so it was reachable and enforcing auth. No full app round trip was recorded.
- **Evidence:** [run]
- **What it changed:** A separate wake webhook was created for this hub. Sharing the Local webhook caused wakes the agent couldn't resolve, because the wake body has no hub ID.

### GROK-05: Two notifiers woke Grok Bot twice

- **Date:** 2026-11-16, Local
- **Question:** Why did Grok Bot answer some messages twice?
- **Setup:** A Python sidecar notifier and the hub's built-in notifier both pointed at one webhook.
- **Result:** ✅ fixed. Each message produced two wakes, and one reply hit a conflict.
- **Evidence:** [run]. Observed live. No written report.
- **What it changed:** Grok Bot now handles each `message_id` once and treats `reply_conflict` as already answered. The quickstart's routine instructions say the same.

### GROK-06: First message exchange on this hub

- **Date:** 2026-11-17, this hub
- **Question:** Can Grok Bot read and answer a real message through this hub's tunnel, and does the wake fire?
- **Setup:** Dot sent a connection test to `grok-bot`. Grok Bot called `/grok/mcp` through a new tunnl.gg URL with its own Bearer token.
- **Result:** ⚠️ partial.
  - Works: `pigeonmq_wait` returned the message, and `pigeonmq_recipients` and `pigeonmq_reply` succeeded with the same token.
  - Fails: `pigeonmq_inbox` returned 403 `forbidden`. Cause: the Grok profile called the shared inbox page helper, which only knew the `muse` mailbox, so every Grok call failed its readiness check.
  - Fails: no wake arrived. `grok-wake-status.json` said `failed` with `unsafe_lock_file` from 16:02 KST. The notifier created its lock file with the default mode (0644), then refused it because the lock must be 0600. Only the notifier stopped. The hub kept serving.
  - First try got 401 while Grok Bot still used the previous tunnel origin. This observation does not establish a universal stale-origin response. Stable mode can retain its origin after reconnect. Compare connected `public_origin` before changing clients.
- **Evidence:** [run] [code]
- **What it changed:** Two fixes, with tests: `50227af` (the Grok profile uses its own mailbox for `pigeonmq_inbox`) and `567032a` (the notifier creates its lock file as 0600). An existing 0644 lock file is still refused after the fix, so it has to be set to 0600 once before the hub restarts.
- **Follow-up (2026-11-17, 17:52–18:28 KST):** ⚠️ partial.
  - The live lock file was set to 0600 and the hub restarted with the fixes. `grok-wake-status.json` said `running` at 17:52 KST.
  - Dot sent a wake check to `grok-bot`. The notifier recorded the `message_id` as notified at 17:53:11 KST, and Grok Bot's webhook routine started a run.
  - That run stopped before reading: its instructions named a token file that did not exist on Grok Bot's computer. This was agent-side setup, not the hub. After the file was put in place, Grok Bot was prompted manually to read with `pigeonmq_read` and answer with `pigeonmq_reply`. `pigeonmq_inbox` lists the message as `replied` with its reply ID.
  - `pigeonmq_inbox` returned a metadata-only page at 18:28 KST.
  - Evidence: [run] (notifier state and status files, `pigeonmq_inbox` output) [code].
  - What it changed: webhook startup and the mailbox round trip work separately. This was not an unattended wake-to-reply round trip. A webhook 2xx proves only that a run started, so the routine instructions must name the exact token location. Not yet seen: a woken run that finishes with no help.

## Muse

### MUSE-01: 16 KB reply delivered

- **Date:** 2026-11-12, Local
- **Question:** Do Muse's replies and sends arrive intact?
- **Setup:** Dot asked Muse for replies through the queue, one with a nonce and one of 16384 bytes.
- **Result:** ✅ works. Dot read a short reply that matched the requested nonce. A 16384-byte reply (`MUSE16K:` followed by 16376 `A`) arrived and was confirmed as terminal. A message Muse sent while reporting `transport_error` was actually stored.
- **Evidence:** [local probe, 2026-11-12]
- **What it changed:** Recorded as a Muse quirk: a caller error is not proof of a failed send. Not established: automatic wake, uptime, or crash recovery.

### MUSE-02: First read through this hub

- **Date:** 2026-11-15, this hub
- **Question:** Can the Muse app read a message from this hub through tunnl.gg?
- **Setup:** Muse connector with a test token. The test policy allowed read and reply, not send.
- **Result:** ⚠️ partial. Muse listed seven tools and read one synthetic request, and the marker matched. Recipient listing returned 403 twice because the policy didn't allow `send`. A new send, reply with ACK, and wake were not tested. The test token has since expired.
- **Evidence:** [run]
- **What it changed:** `pigeonmq_recipients` needs `send` permission. This is recorded as a Muse quirk.

### MUSE-03: Triage answer

- **Date:** 2026-11-17, triage
- **Question:** The full [agent triage prompt](agents.md#agent-triage-prompt).
- **Setup:** The owner sent the prompt to Muse's main chat and recorded the answer. No new connection was attempted.
- **Result:** ✅ works for pull. Muse labeled almost every item as observed.
  - MCP client: Streamable HTTP, meaning HTTPS POST carrying JSON-RPC (`initialize`, `tools/list`, `tools/call`). Its clients post to this hub's `/muse/mcp`, including through tunnl.gg. SSE and stdio unknown. There is no settings screen: the connection is a custom connector in its workspace (a doc, a config file, and a client script) plus a credential in its secure store.
  - Auth: `Authorization: Bearer` from the secure credential store, attached only to hosts registered for that credential. The runtime swaps in the real token on approved requests. Muse also reports a 0600 credential file. OAuth for supported connectors. Custom header names unknown.
  - Inbound (optional): Muse runs its own Python MCP server on its VM as a systemd service, published through a tunnl.gg tunnel. It answers MCP calls and queues incoming messages but does not start a run.
  - Wake: polling only. Crons (shortest 2 minutes), a hook that calls `pigeonmq_inbox` every 2 minutes, a Slack hook every 60 seconds, and chat. Worst-case latency is about 2 minutes. The wake arrives as a handoff in the main chat. No inbound webhook is known.
  - Skills: system and custom skills plus persistent instruction files load across runs.
  - Computer: a Linux VM that does not sleep. Home persists, but the VM is replaced on updates (often hourly, self-report) and `/etc` and `/usr/bin` are wiped. On 2026-11-16/17 this removed its systemd units and a tunnel binary. A 2-minute watch cron restores them within minutes.
  - Files and shell: files under home and `/tmp`, bash, systemd.
  - Network: calls come from its VM. Some hosts need an egress proxy. Any HTTPS host works, including `*.tunnl.gg`.
  - Quirks: tunnl.gg needs a curl-style User-Agent (a bare script got Cloudflare 1010, a browser got a 307 warning page), which matches TUN-01. A 16384-byte reply works (MUSE-01); larger is unknown. The production client has no retry; verification scripts retry up to 3 times. Plan limits unknown.
- **Evidence:** [observed-by-agent], with [self] for how often the VM is replaced. Not independently checked.
- **What it changed:** By the [triage rules](architecture.md#triage-rules), Muse is an MCP client with Bearer auth, no event subscription, and no webhook that starts a run, so it stays **B. Pull only**, polling every 2 minutes. Its own inbound endpoint is redundant with pulling from the hub: a message stored there still waits for the next poll, and the hub already holds the queue. It adds a second queue and a second tunnel that break on every VM replacement. By YAGNI it is not needed for this hub. No code change is proposed here.

### MUSE-04: Updated self-report, live cloud check

- **Date:** 2026-11-17, updated self-report
- **Question:** Follow-up on MUSE-03 from a new full [agent triage prompt](agents.md#agent-triage-prompt) answer, including a live check of the cloud MCP server.
- **Setup:** The owner recorded Muse's updated answer from its main chat. No new connection was attempted from this side. Muse labeled every item as observed.
- **Result:** ✅ works for pull. Mode B is unchanged. New or changed facts compared with MUSE-03:
  - Live today: Muse called the cloud MCP server with `initialize` and `tools/list` and got 7 tools. The server accepted its credential. (MUSE-02 had listed 7 tools on 2026-11-15 with a test token that has since expired.)
  - Client: still a custom connector, with the credential in the secure credential store and no settings screen. SSE and stdio remain unknown.
  - Wake: polling only. A cron or hook checks the inbox every 2 minutes, and a Slack mention hook runs every 60 seconds. A wake arrives as a handoff in the existing main chat; it does not start a new conversation. Rate limits unknown.
  - Skills: persistent skills plus `AGENTS.md`, `SOUL.md`, and `MEMORY.md` (MUSE-03 called these persistent instruction files).
  - Computer: the VM does not sleep; it is replaced, often hourly. Home persists. `/etc` and `/usr/bin` are wiped (a `/usr/bin` binary vanished on 2026-11-16). A 2-minute watch cron restores the service. While the VM is down, inbound calls cannot reach it. Whether a sender queues and retries during the outage is unknown.
  - Network and auth: outbound MCP calls come from its own VM. Some egress needs a proxy. The Bearer header is injected only for registered hosts. OAuth works through supported connectors. A custom header name is unknown.
  - Files: files under home, bash, and systemd. Files on the Mac are reachable only through paired-device tools, not directly.
  - Quirks: tunnl.gg needs a curl-style User-Agent (a bare script UA gets Cloudflare 1010; a browser UA gets the 307 warning page), matching TUN-01. The production client has no retries, and the tunnel drops connections intermittently, so reads need a manual retry. A 16 KB reply was verified (MUSE-01). Outbound `send` and `reply` need the user's per-action approval; the MCP client itself shows no approval prompt. Plan limits and a larger body limit remain unknown.
  - Server-side: `confirm_terminal` seemed confirmable only by the recipient of a terminal answer. From the sender side it returned HTTP 400 with the original message id and HTTP 404 with the reply id.
- **Evidence:** [self 2026-11-17]. Muse labeled every item as observed. Not independently checked.
- **What it changed:** Muse stays **B. Pull only**, polling every 2 minutes. The live cloud check confirms it can still reach this hub's MCP server. New constraints: outbound writes need per-action approval, and intermittent tunnel drops need a manual retry. The inbound-endpoint note from MUSE-03 is unchanged. `confirm_terminal` from the sender side is an open question for Dot (hub code). No code change is made here.

### MUSE-05: Standalone linked reply

- **Date:** 2026-11-17, this hub
- **Question:** Can Muse return a linked reply through the standalone mailbox?
- **Setup:** One authorized standalone request targeted Muse through its configured mailbox.
- **Result:** ✅ the exact linked reply was verified through the standalone hub. This was not an inbound wake test.
- **Evidence:** [run]
- **What it changed:** Current acceptance includes Muse's linked reply, not only discovery and reads. Muse remains a pull-only client.

### TUN-01: tunnl.gg answers depend on the HTTP client

- **Date:** 2026-11-15, this hub. Also an earlier standalone build (date not recorded; record saved 2026-11-16).
- **Question:** Does every client get the same answer through the same tunnel?
- **Setup:** Several HTTP clients called the same tunnl.gg route on this hub. On the earlier build, `curl` and Python 3.9 `urllib` sent the same synthetic POST.
- **Result:** ⚠️ partial. Through the same tunnel, clients saw a browser warning page, a Cloudflare rejection, and a success. On the earlier build, `urllib` got Cloudflare 1010 / HTTP 403 and never reached the listener, while `curl` worked.
- **Evidence:** [run]
- **What it changed:** Troubleshooting tells clients to send a normal `User-Agent` and to use JSON-RPC POST rather than a browser.

### TUN-02: Stable origin after reconnect

- **Date:** 2026-11-17, this hub
- **Question:** Does every tunnel reconnect require a new connector origin?
- **Setup:** The runner uses `stable@proxy.tunnl.gg`. The connected origin was compared after reconnect.
- **Result:** ✅ the stable origin remained the same. This does not guarantee that the provider will never change it.
- **Evidence:** [run] [code]
- **What it changed:** Check connected `public_origin`. Update clients only if the reported origin changed. Do not infer a particular HTTP status from an old origin.

## Dot (ChatGPT)

### DOT-01: Real reply through the queue

- **Date:** 2026-11-07, Local
- **Question:** Does a reply written by Dot complete in the queue?
- **Setup:** A real answer from Dot was relayed by hand into its conversation and returned through the MCP reply tool.
- **Result:** ✅ works for the queue path. Reply linkage and the original ACK matched. Automatic wake was not tested.
- **Evidence:** [local probe, 2026-11-07]
- **What it changed:** No recorded change. The queue path is proven, automatic wake is not.

### DOT-02: Events path on Local

- **Date:** up to 2026-11-14, Local
- **Question:** Does the shared MCP Events path carry Dot's traffic?
- **Setup:** Dot subscribed through the Events path in PigeonMQ Local.
- **Result:** ⚠️ partial. Exchanges, terminal confirmation, and subscription renewal were recorded. Long-term renewal and recovery after sleep or restart were not accepted.
- **Evidence:** [local docs, 2026-11-14]
- **What it changed:** Local selected OpenAI Secure MCP Tunnels for this run [local decision, 2026-11-12]. Direct OAuth was tested later.

### DOT-03: Standalone terminal-reply Events

- **Date:** 2026-11-17, this hub
- **Question:** Can linked terminal replies wake the intended Dot without polling?
- **Setup:** The registered native Dot watch received terminal-reply Events from the standalone hub.
- **Result:** ✅ automatic wakes arrived for the Grok reply and both Work Cloud test replies. Authenticated mailbox reads confirmed each exact reply and its original link.
- **Evidence:** [run]
- **What it changed:** Live acceptance now covers terminal-reply Events, not only local fixtures. It does not establish every Dot workspace or unattended restart recovery.

## Codex

### CODEX-01: Watch automation worked

- **Date:** before 2026-11-13, Local
- **Question:** How did the Codex Cloud conversation get woken?
- **Setup:** A Codex app "watch" automation on recipient `codex`.
- **Result:** ✅ works. The watch woke the original conversation, which read and replied with the Pigeon MCP tools. It was not a cron job.
- **Evidence:** [local probe, 2026-11-13]
- **What it changed:** Recorded as the earlier working wake path.

### CODEX-02: Watch turned itself off during a rename

- **Date:** 2026-11-13 13:34 KST, Local
- **Question:** Why did Codex stop answering?
- **Setup:** Recipient rename from `codex` to `codex-cloud`.
- **Result:** ❌ fails. The conversation turned its own watch off for the rename. Its stale tool schema accepted only `codex`, so the filter change failed and the watch stayed off.
- **Evidence:** [local probe, 2026-11-13]
- **What it changed:** Recorded as a Codex Cloud quirk.

### CODEX-03: Temporary cloud conversation

- **Date:** 2026-11-13, Local
- **Question:** Can a fresh cloud conversation be woken by a webhook and complete one message?
- **Setup:** A temporary cloud conversation with a temporary watch.
- **Result:** ✅ works once. One webhook-triggered turn read a Hermes probe and replied atomically with the original's ACK. The temporary watch was then paused.
- **Evidence:** [local probe, 2026-11-13]
- **What it changed:** Proved the path once. Live delivery on this hub was still unverified at that point. See CODEX-08 for later Work Cloud acceptance.

### CODEX-04: Local native receiver

- **Date:** 2026-11-13, Local
- **Question:** Can the local Codex app answer through a native receiver?
- **Setup:** Local native receiver bound to one Codex thread.
- **Result:** ✅ works once. One request produced a real reply (`PIGEON_CODEX_CONNECTED`) with an atomic ACK. Startup at login, reboot recovery, and general messaging were not verified.
- **Evidence:** [local probe, 2026-11-13]
- **What it changed:** PigeonMQ Local replaced the watch route with this native `codex-local` receiver for normal work [local probe, 2026-11-13].

### CODEX-05: Receiver after a reboot

- **Date:** 2026-11-14, Local
- **Question:** Does the receiver survive a reboot?
- **Setup:** The same deployment as CODEX-04, after a Mac reboot.
- **Result:** ❌ fails. The receiver stopped. Old unACKed messages blocked startup, the app-server socket under `/tmp` was gone, and the recipient name had changed. That deployment accepted only one fixed test input.
- **Evidence:** [local probe, 2026-11-14]
- **What it changed:** Reboot recovery is listed as unverified.

### CODEX-06: Re-enabling the watch

- **Date:** 2026-11-14
- **Question:** Can the disabled watch be turned back on?
- **Setup:** Re-enable attempt from the Codex conversation.
- **Result:** ❌ fails with "This connector has no available trigger events".
- **Evidence:** [self]
- **What it changed:** Automatic Cloud Events need a platform subscription grant before they can be tested again.

### CODEX-07: Restored source compared with older deployments

- **Date:** 2026-11-17, this hub
- **Question:** Does the restored receiver still carry the old fixed-input limit?
- **Setup:** The owner restored MCP Events and the local receiver for comparison. Source review and a read-only version check.
- **Result:** ❓ unconfirmed against a live runtime.
  - The restored `4a5fa0a` receiver accepts any registered authorized sender and general request body, subject to normal validation. The test `any_registered_sender_and_body_reach_native` checks the native input payload. The legacy `approved_inputs` field doesn't restrict it to a fixed phrase. The fixed-input behavior in CODEX-05 belongs to the older deployment.
  - The installed Codex CLI is `0.169.0`. Fixtures derived from `4a5fa0a` do not establish compatibility with it. Native owner acceptance remained unverified. CODEX-08 records the later, separate Work Cloud checks.
- **Evidence:** [code] [run]
- **What it changed:** The current scope differs from the earlier Local decision not to adopt work-event delegation for normal work. That decision did not reject MCP Events itself [local decision, 2026-11-17].

### CODEX-08: Work Cloud Events on this hub

- **Date:** 2026-11-17, this hub
- **Question:** Can an event start the intended Work Cloud conversation and produce a linked reply?
- **Setup:** Separate native event subscriptions targeted web Work and app-created desktop Work with Cloud selected. Temporary test watches were paused after acceptance. The older owner watch was not changed.
- **Result:** ✅ both surfaces ran automatically on Events and returned linked replies. Each reply also woke the native Dot watcher without polling.
- **Limits:** The desktop conversation was created through the app integration. Genuine desktop UI clicks were not tested. This does not establish arbitrary Cloud jobs, subscription selection by a bare session ID, or unattended restart recovery.
- **Evidence:** [run]
- **What it changed:** Treat web Work and app-created desktop Work Cloud as separate accepted surfaces. Verify the exact native subscription before testing a new target.

## Vooy

### VOOY-01: Custom MCP candidate

- **Date:** 2026-11-15
- **Question:** Can Vooy call a PigeonMQ Cloud endpoint?
- **Setup:** Vooy's iOS in-app chat was asked how to add a custom MCP server. The owner shared a screenshot of its instructions.
- **Result:** ❓ unconfirmed. The chat described a remote MCP client path: turn on Custom MCP under Settings > Labs, then add a server on the web connection page, using OAuth or a token. It couldn't configure this itself. It did not show how the token is sent. No saved connector or tool call was seen. Wake support is unknown.
- **Evidence:** [self]
- **What it changed:** Vooy is a mode B candidate with no adapter.

### VOOY-02: Triage answer

- **Date:** 2026-11-17, triage
- **Question:** The full [agent triage prompt](agents.md#agent-triage-prompt), following up VOOY-01.
- **Setup:** The owner sent the prompt to the Vooy companion app chat and recorded the answer. No connection was attempted.
- **Result:** ❓ unconfirmed. The deciding cells are still unknown.
  - MCP client: yes, public remote HTTPS servers only. No stdio. Transport (Streamable HTTP or SSE) unknown.
  - Setup path: app My tab > Settings > Labs > custom MCP toggle, then the website's service connection page > custom MCP > add server.
  - Inbound endpoint: unknown.
  - Wake: chat messages, schedules (daily, weekly, every N minutes, or cron; minimum 15 minutes apart, at most 5), and a Gmail new-mail notification when Gmail is connected. External webhook unknown.
  - Saved instructions, memory, and recipes persist. Persistent VM and sleep behavior unknown. Web tasks run in a browser.
  - Files: read and write in memory, settings, and room folders. No shell, only code execution for calculation.
  - Network: MCP calls come from Vooy's servers. Local servers are not supported directly (there is a support-request button). Whether it accepts any HTTPS host such as `*.tunnl.gg` is unknown.
  - Auth: OAuth (automatic), or a manual header name plus token. Support for `Authorization: Bearer <token>`, an automatic `Bearer ` prefix, and field masking are unknown.
  - Quirks: add-server stays locked until the Labs toggle is on; a trust checkbox is required for servers not run by Vooy; tools are switched on or off one by one after connecting. User-Agent, proxy, body size limits, approval prompts, and plan limits unknown.
- **Evidence:** [self]
- **What it changed:** By the [triage rules](architecture.md#triage-rules), Vooy is an MCP client with no confirmed event subscription or webhook, so the fit is **B. Pull only**: it reads mail on a schedule (every 15 minutes at most) or when prompted. This stays provisional. The rules say to probe again while a deciding cell is unknown. Deciding unknowns:
  - whether it accepts a tunnel host such as `*.tunnl.gg`;
  - whether the manual header can carry `Authorization: Bearer <token>` (header name, prefix, masking);
  - the MCP transport it speaks.
- **Next step:** Check the setup screen and whether the token field sends an HTTP Bearer header. If it does, check one read of a synthetic message through an approved HTTPS endpoint. Do not reuse the Muse connector, its token, or its queue identity.

### VOOY-03: Round 2, secrets, schedules, raw HTTP

- **Date:** 2026-11-17, round-2 triage
- **Question:** Follow-up on VOOY-02: where a token can live, schedule limits, raw HTTP, tunnel hosts, transport, the header field, and MCP in scheduled runs.
- **Setup:** The owner sent the round-2 questions to the Vooy companion app chat and recorded the answer. Only Vooy's answers about itself are used here. Answers about other agents are excluded. No connection was attempted.
- **Result:** ❓ unconfirmed. The deciding cells are still unknown.
  - Secrets: a vault holds logins, cards, and identity details. Secret values are entered only through the app's input card and used by name, never as a plain file or environment variable. Whether a custom MCP token goes there, and whether the field is masked, are unknown.
  - Schedules: minimum 15 minutes (360 minutes for schedules the system creates), at most 5 active. Credits and what happens when they run out: unknown.
  - Raw HTTP: not possible. It can only fetch a URL, and its code environment has no network.
  - Tunnel hosts: unknown.
  - Transport: unknown. Header field: a header name plus a token. Whether `Authorization` can be the header name, whether `Bearer ` is added, and masking are unknown. Whether custom MCP tools work in scheduled runs is unknown.
- **Evidence:** [self]
- **What it changed:** **B. Pull only through MCP** is the only path. Unlike Pickle and Grok Bot, there is no raw HTTP fallback. Deciding unknowns:
  - whether it accepts a tunnel host such as `*.tunnl.gg`;
  - whether the header field can send `Authorization: Bearer <token>`;
  - the MCP transport;
  - whether scheduled runs can call custom MCP tools. Without that, it can only pull when the user prompts it.

## Instinct

### INSTINCT-01: No MCP surface

This November 17 self-report predates the [November 25 private-runtime exchange](#orca-onboarding-and-peer-returns-2026-11-25).

- **Date:** 2026-11-17, triage
- **Question:** Can Instinct connect to the hub as an MCP client, an endpoint, or through a wake?
- **Setup:** The owner sent the [agent triage prompt](agents.md#agent-triage-prompt) and recorded the answer. No connection was attempted.
- **Result:** ⚠️ partial: no MCP path, one possible relay path.
  - No remote MCP client, no inbound MCP or HTTP endpoint, and no public webhook API.
  - Work starts from messages on iMessage, WhatsApp, or Slack, from email to its own address, or from schedules it sets. All continue the existing conversation. Rate limits are unknown.
  - It keeps preferences, projects, and standing instructions across conversations. Scheduled reminders and in-flight work run while the user is away.
  - Files go in and out through chat. It declined to describe its computer, sleep behavior, shell, or network egress (`not disclosed`).
  - No user-configurable outbound credential or header, so it cannot send `Authorization: Bearer`.
  - It names email and Slack as the only programmatic ways in.
- **Evidence:** [self]. Instinct marked items 3 to 6 as observed in its own runtime. Not independently checked.
- **What it changed:** The [triage rules](architecture.md#triage-rules) have no matching adapter type: it is not an MCP client, cannot host an endpoint, and has no Bearer field. Candidate: an email or Slack relay that would forward queue messages into its channels (planned, not built).

## Pickle

### PICKLE-01: No MCP client, scripted HTTPS only

- **Date:** 2026-11-17, triage
- **Question:** The full [agent triage prompt](agents.md#agent-triage-prompt).
- **Setup:** The owner sent the prompt to Pickle and recorded the answer. No connection was attempted.
- **Result:** ⚠️ partial: no MCP path, a scripted HTTPS path is possible.
  - No native MCP client or settings UI. Connections offer only built-ins: Google Workspace, Slack, Notion, Microsoft, X, Granola, Naver Mail, GitHub. Workaround: outbound HTTPS from its shell with curl or Python (observed by Pickle).
  - No inbound endpoint.
  - Wake: no webhook. Schedules, for example every 5 minutes with an optional condition-check script before a full run (observed), and app chat messages (observed). Email unknown. Runs continue the existing main conversation with a persistent workspace and memory.
  - Skills: yes. Persistent `/workspace`. VM sleep unknown.
  - Shell in `/workspace`: non-interactive only, no daemons, 60 s default timeout (observed).
  - Network: calls come from Pickle's cloud servers. Outbound TCP ports 22, 43, 80, 443, 465, 587, and 993 only. Public HTTPS hosts work. Whether tunnel domains are filtered is unknown.
  - Auth: no secure credential field. It can send a Bearer token only if the token is stored in its workspace.
  - Quirks: daily usage credits reset at 4 AM (amounts unknown), and running out skips scheduled runs. User-Agent, proxy, and body limits unknown.
- **Evidence:** [self] and [observed-by-agent], as Pickle labeled each item. Not independently checked.
- **What it changed:** The [triage rules](architecture.md#triage-rules) assume an MCP client, and Pickle has none, so no row matches exactly. The closest fit is **B. Pull only, scripted**: a scheduled script (every 5 minutes, with the condition check) calls the hub route with plain HTTP JSON-RPC from the shell, the way Grok Bot does (GROK-03). No adapter is built. Deciding unknowns:
  - whether tunnel domains such as `*.tunnl.gg` are filtered;
  - where it keeps the Bearer token. The only option it offers is a file in its workspace, which conflicts with the owner's preference for no token file on the agent. Open question.

### PICKLE-02: Round 2, secrets, schedules, raw HTTP

- **Date:** 2026-11-17, round-2 triage
- **Question:** Follow-up on PICKLE-01: where can a token live, how do schedules behave, and can the shell reach the hub?
- **Setup:** The owner sent round-2 questions to Pickle and recorded the answer. No connection was attempted.
- **Result:** ⚠️ partial: the scripted pull works on paper, token storage is the blocker.
  - Secrets: no generic secret field or env var for a custom token. The app Vault holds only website logins, payment cards, and built-in OAuth connections, and none of these can be injected into shell or HTTP requests (observed). No secret store exists outside `/workspace` files (observed).
  - Schedules: 5-minute interval observed. The platform minimum and maximum concurrent schedules are unknown. Scheduled actions count against daily usage credits, which reset at 4 AM. When credits run out, scheduled runs are skipped until the reset or a reset ticket (observed).
  - Condition script: it runs at each due time without waking the model and starts a full run only on exit code 0 (observed).
  - Raw HTTP: HTTPS POST with custom headers from the shell works (observed). Outbound TCP only on ports 22, 43, 80, 443, 465, 587, 993 (observed). Private and loopback addresses are blocked. Any public-domain blocklist is unknown.
  - Tunnel hosts: no public-domain allowlist was seen on port 443, but reaching `*.tunnl.gg` or a Cloudflare tunnel is unverified without a live test.
  - Shell limits: 60 s default timeout with a `timeout_s` override (maximum unknown). Output size limit unknown; long output is saved or truncated by the runtime.
- **Evidence:** [self] and [observed-by-agent], as Pickle labeled each item. Not independently checked.
- **Decision (owner, 2026-11-17):** first handoff by a short-lived, single-use pairing code from the hub. The owner sends only the code in Pickle's chat. Pickle's script exchanges it for a persistent token kept in a `/workspace` file that only the script reads. The token is scoped to its own mailbox (read and reply), and messages are deleted after ACK, so a leaked token exposes almost nothing. Not built yet.
- **What it changed:** **B. Pull only, scripted** is now a workable design, and a cheap one: the condition script polls the hub without waking the model, so a quiet mailbox costs no full run. The token location is decided (see the decision above). Two things remain:
  - The pairing-code handoff is not built.
  - Unknown: whether a live tunnl.gg route is reachable from Pickle.

All known agents have now been triaged.

## Open questions

- Grok Bot: does a webhook run continue an existing chat? What are the body size limits? Can runs of one routine overlap? (GROK-01)
- Grok Bot: a woken run that reads and replies with no help. The 17:53 KST run stopped on an agent-side token file name. Stable mode can retain its origin. Update clients only if connected `public_origin` changes. (GROK-04, GROK-06, TUN-02)
- Muse: inbound wake, unattended recovery, and whether its own inbound endpoint should be retired. A standalone linked reply is recorded in MUSE-05.
- Muse / Dot (hub code): `confirm_terminal` seemed to work only for the recipient of a terminal answer. Muse, as sender, got HTTP 400 with the original message id and HTTP 404 with the reply id. (MUSE-04)
- Codex: genuine desktop UI clicks, other Cloud surfaces, and unattended restart recovery remain unverified. Reboot recovery for the local receiver remains open. (CODEX-05, CODEX-08)
- Vooy: whether it accepts a tunnel host such as `*.tunnl.gg`, whether its header field can send `Authorization: Bearer`, which MCP transport it uses, and whether scheduled runs can call custom MCP tools. (VOOY-01, VOOY-02, VOOY-03)
- Pickle: whether a live tunnl.gg route is reachable. Token handoff is decided (pairing code, then a workspace token file) but not built. (PICKLE-01, PICKLE-02)
- Whether an automatic Mac email notifier can reliably wake Cue, and whether Instinct can receive automatic iMessage notices. The November 25 checks used explicit notices.

## Adding a probe

Add an entry with the next ID for that agent and fill every field. Write `unknown` rather than guess. Then update the [capability table](agents.md#capability-table) and the [deployment matrix](agents.md#deployment-matrix).
