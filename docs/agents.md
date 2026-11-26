# Agents and capabilities

Find the connection method, authentication, and test results for each agent.

- [Capability table](#capability-table): what each agent can do
- [Deployment matrix](#deployment-matrix): agent × where it runs → connection mode and status
- [Agent triage prompt](#agent-triage-prompt): the copy-paste prompt for a new agent ([agent triage](architecture.md#agent-triage) step 1)
- [Adding a result to the table](#adding-a-result-to-the-table)

The dated probes behind these cells are in the [research log](research.md). Codex paths are described in [architecture](architecture.md#codex-paths-and-shared-operations).

**How to read cells.** `yes` / `no` / `unknown`, then a source tag in brackets. `unknown` means nobody has probed it. It never means "probably". `not disclosed` means the agent was asked and declined to say. `[observed-by-agent <date>]` means the agent says it saw this in its own runtime; nobody else checked it. Connection modes A–D are defined in [architecture](architecture.md#connection-modes).

Source tags:

| Tag | Meaning |
| --- | --- |
| [off] | Vendor documentation |
| [run] | Observed by the agent in its own runtime, or a live test |
| [self] | The agent said so; not independently checked |
| [code] | This repository's code |
| [local probe, date] | A dated probe in PigeonMQ Local, the private sibling project. Details in the [research log](research.md). |
| [local decision, date] / [local docs, date] | A PigeonMQ Local decision record or doc, by its last update |

## Capability table

| Agent | Calls a remote MCP server? | Can host inbound MCP? | Wake trigger (webhook/event)? | Skills? | Persistent computer / sleeps? | Files & shell? | Needs public URL / tunnel? | Auth it can send | Known quirks |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **Grok Bot** (cloud-hosted) | yes: custom remote MCP with static headers [run] [local probe, 2026-11-10]; in practice calls the route with direct HTTP JSON-RPC from its box [self] [local probe, 2026-11-10] | unknown | yes: routine webhook, `Authorization: Bearer` [off] | yes [run] | yes, workspace persists across runs [run]; a webhook starts a fresh run, so no idle process has to stay awake [off] | yes, on its hosted computer [run] | yes; MCP calls come from Cursor-side servers / shared egress IPs [off] | static `Authorization` header [run] | Needs a `User-Agent` header through the tunnel [self]. Webhook 200 = run started, not finished [off]. 300 runs/h total, 200/h per routine; over-limit runs fail and are not retried [off]. Automatic tool review can block a routine with no one to approve [self]. Wake has no hub ID → one webhook per hub [code]. |
| **Grok Bot** (desktop-attached) | same as cloud-hosted | unknown | same as cloud-hosted | yes [run] | same as cloud-hosted | also on the user's registered Mac, each command approved by the user [run] | same as cloud-hosted for the MCP route | same | Can use shell tools on the registered Mac. CLI `send` and `reply` are disabled in this release [code]. |
| **Muse** (main chat, Linux VM) | yes: Streamable HTTP (HTTPS POST with JSON-RPC) to custom remote MCP servers, including this hub through tunnl.gg [observed-by-agent 2026-11-17] [run 2026-11-15]; on November 17, `initialize` + `tools/list` returned 7 tools on the cloud MCP server [self 2026-11-17]. SSE and stdio unknown. No settings screen: a custom connector in its workspace plus a credential in its secure store [observed-by-agent 2026-11-17] | yes, optional: its own Python MCP server on its VM behind a tunnl.gg tunnel; it only queues incoming messages and does not start a run [observed-by-agent 2026-11-17] | polling only, no push: a cron or hook checks the inbox every 2 min, a Slack mention hook every 60 s, and chat; about 2 min worst-case latency; a wake arrives as a handoff in the existing main chat and does not start a new conversation [self 2026-11-17]; no inbound webhook known; rate limits unknown | yes: persistent skills plus `AGENTS.md`, `SOUL.md`, and `MEMORY.md` [self 2026-11-17] | Linux VM that does not sleep; it is replaced, often hourly; home directory persists; `/etc` and `/usr/bin` are wiped (a `/usr/bin` binary vanished on 2026-11-16); a 2-min watch cron restores the service [self 2026-11-17]. Inbound calls cannot reach it while it is down; whether a sender queues and retries during the outage is unknown [self 2026-11-17] | yes: files under home and `/tmp`, bash, systemd [observed-by-agent 2026-11-17]; Mac files only through paired-device tools, not directly [self 2026-11-17] | calls come from its own VM; some hosts need an egress proxy; any HTTPS host including `*.tunnl.gg` works [observed-by-agent 2026-11-17]; a public URL is needed only for its optional inbound endpoint | `Authorization: Bearer` from its secure credential store, attached only to registered hosts; the runtime swaps in the real token on approved requests [observed-by-agent 2026-11-17]. The cloud MCP server accepted the credential in a live check today [self 2026-11-17]. It also reports a 0600 credential file. OAuth for supported connectors; custom header names unknown | tunnl.gg needs a curl-style User-Agent (bare script: Cloudflare 1010, browser: 307 warning page) [observed-by-agent 2026-11-17]. 16384-byte reply OK, larger unknown. Production client has no retry, and the tunnel drops connections intermittently, so reads need a manual retry [self 2026-11-17]. Outbound `send` and `reply` need the user's per-action approval; the MCP client itself shows no approval prompt [self 2026-11-17]. VM replacement wipes `/etc` and `/usr/bin`, so recovery must restore binaries too [observed-by-agent 2026-11-17]. `radio999k_recipients` needs `send` permission (403 otherwise) [run]. A send reported `transport_error` but was actually stored [local probe, 2026-11-12]. `confirm_terminal` from the sender side returned HTTP 400 (original message id) and 404 (reply id) [self 2026-11-17]. Triage: [MUSE-03](research.md#muse-03-triage-answer), [MUSE-04](research.md#muse-04-updated-self-report-live-cloud-check). |
| **Dot** (ChatGPT) | yes: private ChatGPT MCP app [run] | unknown | Terminal-reply MCP Events verified through the private tunnel. Automatic Events on direct OAuth remain unverified [run] | unknown | unknown | unknown | Direct HTTPS or a private OpenAI Secure MCP Tunnel [run] | Cloudflare Access Managed OAuth with a signed assertion, or a tunnel-injected Bearer [run] [code] | Direct OAuth needs a dedicated Access app and ChatGPT approval. Organization-managed Dot workspaces are unverified. [New Orca installation](research.md#new-orca-installation-with-a-hosted-address) |
| **Codex** (local app) | Common hub client [code] | no: local app-server socket [code] | Local native worker, mode D [code] | unknown | Runs on the Mac [code] | yes | no | Bearer for the common receiver API [code] | Mailbox `codex-local`. Protected binding fixes the existing thread, native socket, working directory, and registration. Local tests do not establish Cloud acceptance. |
| **Codex** (cloud conversation) | yes: web Work and app-created desktop Work Cloud [run 2026-11-17] | unknown | yes: automatic event runs and linked replies on both Work Cloud surfaces [run 2026-11-17] | unknown | unknown | unknown | depends on the OpenAI route | unknown | The conversation disabled its watch on 2026-11-13 during a recipient rename [local probe, 2026-11-13]. Re-enable failed 2026-11-14 with "This connector has no available trigger events" [self]. Work delegation was not adopted [local decision, 2026-11-17]. Separate standalone Work Cloud acceptance succeeded in [CODEX-08](research.md#codex-08-work-cloud-events-on-this-hub). Native desktop UI clicks were not tested. |
| **Hermes** | gateway client [local decision, 2026-11-12] | webhook/API disabled in its setup [local probe, 2026-11-06] | via its gateway (PigeonMQ Local) | unknown | runs on the Mac | yes | no | n/a | PigeonMQ Local only; no adapter in this repository. |
| **Pickle** | no native MCP client or settings UI; Connections offer only built-ins (Google Workspace, Slack, Notion, Microsoft, X, Granola, Naver Mail, GitHub) [self 2026-11-17]. Workaround: outbound HTTPS from its shell with curl or Python [observed-by-agent 2026-11-17] | no [self 2026-11-17] | no webhook; schedules (minimum 5 min observed, platform minimum and max concurrent unknown); a condition-check script runs at each due time without waking the model and starts a full run only on exit code 0 [observed-by-agent 2026-11-17]; app chat messages [observed-by-agent 2026-11-17]; email unknown. Runs continue the existing main conversation with persistent workspace and memory [self 2026-11-17] | yes [self 2026-11-17] | persistent `/workspace`; VM sleep unknown [self 2026-11-17] | yes in `/workspace`: non-interactive only, no daemons, 60 s default timeout with a `timeout_s` override (maximum unknown); output size limit unknown [observed-by-agent 2026-11-17]. Earlier self-report: no Mac shell [local probe, 2026-11-06] | yes; calls come from Pickle cloud servers; outbound TCP ports 22, 43, 80, 443, 465, 587, 993 only [observed-by-agent 2026-11-17]; private and loopback addresses blocked [self 2026-11-17]; HTTPS POST with custom headers from the shell works and no public-domain allowlist was seen on 443 [observed-by-agent 2026-11-17]; reaching `*.tunnl.gg` or a Cloudflare tunnel unverified | no secure credential field or env var for a custom token; the app Vault holds only website logins, cards, and built-in OAuth, and can't be injected into shell or HTTP [observed-by-agent 2026-11-17]; a Bearer token only from a file in `/workspace` [observed-by-agent 2026-11-17] | Scheduled actions count against daily usage credits (reset 4 AM, amounts unknown); when they run out, scheduled runs are skipped until the reset or a reset ticket [observed-by-agent 2026-11-17]. User-Agent, proxy, and body limits unknown. Generic launcher blocked [local docs, 2026-11-13]. Triage: [PICKLE-01](research.md#pickle-01-no-mcp-client-scripted-https-only), [PICKLE-02](research.md#pickle-02-round-2-secrets-schedules-raw-http). |
| **Vooy** (companion app chat) | yes: public remote HTTPS only; stdio no; transport (Streamable HTTP / SSE) unknown. Set up in app My > Settings > Labs > custom MCP toggle, then the website's service connection page > custom MCP > add server [self 2026-11-17] | unknown [self 2026-11-17] | partial: chat messages, schedules (daily, weekly, every N min, cron; minimum 15 min, system-created 360 min, at most 5 active), Gmail new-mail notification; external webhook and MCP tools in scheduled runs unknown [self 2026-11-17] | yes: saved instructions, memory, and recipes persist [self 2026-11-17] | persistent VM and sleep unknown; web tasks run in a browser [self 2026-11-17] | read/write files in memory, settings, and room folders; no shell, only code execution for calculation, with no network; no raw HTTPS POST with headers, only URL fetch [self 2026-11-17] | yes, public HTTPS; MCP calls come from vendor servers; local servers not supported directly (support-request button); whether it accepts any HTTPS host such as `*.tunnl.gg` unknown [self 2026-11-17] | OAuth (automatic), or a manual header name + token field; `Authorization` as the header name, automatic `Bearer ` prefix, and field masking unknown. A vault holds logins, cards, and identity, entered only through the app's input card and referenced by name; whether a custom MCP token goes there unknown [self 2026-11-17] | Add-server stays locked until the Labs toggle is on. A trust checkbox is required for servers not run by Vooy. Tools are switched on or off one by one after connecting. User-Agent, proxy, body size limits, approval prompts, and plan limits unknown [self 2026-11-17]. Triage: [VOOY-01](research.md#vooy-01-custom-mcp-candidate), [VOOY-02](research.md#vooy-02-triage-answer), [VOOY-03](research.md#vooy-03-round-2-secrets-schedules-raw-http). |
| **Cue** (private runtime) | yes, direct HTTPS MCP [run 2026-11-25] | unknown | manual ID-only email woke the existing app [run] | unknown | unknown | unknown | yes, approved HTTPS route [run] | private Bearer [run] | Native credential import failed on the first attempt. Its enrolled runtime read and replied. |
| **Instinct** (iMessage/WhatsApp/Slack/email) | yes, direct HTTPS JSON-RPC in the enrolled runtime [run 2026-11-25] | no [self 2026-11-17] | no webhook API; messages on iMessage/WhatsApp/Slack, email to its own address, and schedules start work in the existing conversation [self 2026-11-17] | yes, persistent memory of preferences, projects, and standing instructions [self 2026-11-17] | not disclosed; scheduled reminders and in-flight work still run while the user is away [self 2026-11-17] | files in and out through chat; beyond that not disclosed [self 2026-11-17] | approved HTTPS route [run 2026-11-25] | private Bearer obtained through its secure enrollment form [run 2026-11-25] | Manual iMessage notice followed by a linked MCP reply. Automatic iMessage intake is unverified. [November 25 results](research.md#orca-onboarding-and-peer-returns-2026-11-25) |

## Deployment matrix

| Agent | Variant | Mode | Status on this hub | Status elsewhere |
| --- | --- | --- | --- | --- |
| Grok Bot | cloud-hosted / desktop-attached | A. Wake + pull | Webhook started a run (2026-11-17 17:53 KST). A token-file mismatch stopped it. Read and linked reply were completed manually. A November 25 poller and routine picked up an original and posted a linked answer. Project retrieval needed a later check. Separate webhook for this hub ([GROK-06](research.md#grok-06-first-message-exchange-on-this-hub)) | **Verified** on PigeonMQ Local: manual exchanges and independent sends; wake delivered 2xx ~5 s after storage (2026-11-10) [local probe, 2026-11-10] |
| Muse | connector, Linux VM | B. Pull only (polls every 2 min; its own inbound endpoint is optional) | **Verified read** through tunnl.gg (2026-11-15). Muse reported a live `initialize` + `tools/list` (7 tools) on the cloud MCP server (2026-11-17) [self]. A separate standalone linked reply is verified ([MUSE-05](research.md#muse-05-standalone-linked-reply)). The November 25 poller picked up a request after an intervening failure | **Verified** short two-way traffic and a 16384-byte reply (2026-11-12) [local probe, 2026-11-12] |
| Dot | ChatGPT private MCP app | Direct OAuth or private tunnel | Direct OAuth returned a linked project reply after a prompted mailbox check. The private tunnel verified terminal-reply Events. [New Orca installation](research.md#new-orca-installation-with-a-hosted-address) · [DOT-03](research.md#dot-03-standalone-terminal-reply-events) | Local verified exchanges and terminal confirmation [local docs, 2026-11-14] |
| Codex | local app, `codex-local` | D. Common receiver API plus Codex native worker | Worker and API implemented. Live native owner acceptance remains unverified. | **Verified once** in Local: one native reply + ACK (2026-11-13). Historical receiver had a fixed test-input restriction [local probe, 2026-11-13] [local probe, 2026-11-14] |
| Codex | cloud conversation, `codex-cloud` | C. MCP Events | **Verified** automatic event runs and linked replies in web Work and app-created desktop Work Cloud. Native desktop UI clicks untested. Temporary test watches paused ([CODEX-08](research.md#codex-08-work-cloud-events-on-this-hub)). | **Verified once** (2026-11-13). The temporary watch was then disabled. Work delegation was not adopted [local probe, 2026-11-13] |
| Hermes | Mac gateway | Local only | Not applicable | Verified in Local [local docs, 2026-11-14] |
| Pickle | cloud workspace | B. Pull only, scripted: a scheduled condition script polls the hub over HTTP JSON-RPC and wakes the model only on new work. Workable. Token handoff decided 2026-11-17 (pairing code, then a workspace token file; not built). Tunnel reachability unverified | None | Triage, 2 rounds (2026-11-17) |
| Cue | Private app | Email notice, then MCP read and reply | Verified with an explicit notice and an original ACK | Unknown |
| Instinct | Messages conversation | iMessage notice, then MCP read and reply | Verified with an explicit notice and an original ACK | Earlier self-report, November 17 |
| Vooy | web/iOS | B. Pull only through MCP, no raw HTTP fallback (provisional; tunnel host, Bearer header, transport, and MCP in scheduled runs unknown) | None | Triage only (2026-11-15, 2026-11-17 ×2) |

## What we verified (probes)

Moved to the [research log](research.md): dated probes per agent, with setup, result, and evidence.

## Agent triage prompt

Step 1 of [agent triage](architecture.md#agent-triage): **triage prompt → capability row → triage rules → adapter type**.

Paste this into the agent you want to connect. It asks the agent to fill one row of the capability table and to say `unknown` instead of guessing. It does not ask for any URL or token, and it shouldn't change any settings.

```text
I want to connect you to a message queue that runs on my Mac and is reachable
over HTTPS as an MCP server. Before I set anything up, describe what YOU can do.

Rules:
- Answer only from your own runtime, settings screens you can see, or official docs.
- For each answer add a source: "docs: <link>", "observed", or "self-report".
- If you are not sure, write "unknown". Do not guess. Do not change any settings.
- Do not ask me for a URL or a token.
- Reply in ONE message. Put your whole answer (answers + table row) inside ONE
  fenced code block so I can copy it in one tap. Do not split it across messages.

Answer each question, then return ONE markdown table row in exactly this format:

| <product and surface> | <MCP client?> | <inbound MCP?> | <wake trigger?> | <skills?> | <persistent computer / sleeps?> | <files & shell?> | <needs public URL?> | <auth you can send> | <quirks> |

Questions:
1. MCP client: Can you call a custom remote MCP server over HTTPS? Which transport
   (Streamable HTTP, SSE, stdio)? Where is it configured (exact settings path)?
2. Inbound: Can you expose your own MCP or HTTP endpoint that something else calls?
3. Wake: Can an outside event start a run for you (webhook, email, schedule, chat
   message)? What does the request look like (method, headers, body)? Does it
   continue an existing conversation or start a new one? Rate limits?
4. Skills: Can you load saved skills or instructions that persist between runs?
5. Computer: Do you have a persistent computer or VM? Does it sleep or stop when
   idle? If so, what happens to an incoming wake while it sleeps?
6. Files & shell: Can you read/write files and run shell commands? Where?
7. Network: Do your MCP calls come from your own computer or from the vendor's
   servers? Do you accept any HTTPS host (for example *.tunnl.gg)?
8. Auth: Can you send a static "Authorization: Bearer <token>" header from a
   secure credential field? Other options (OAuth, custom header)?
9. Quirks: Anything else that could break a connection (required User-Agent,
   proxies, body size limits, tool-approval prompts, plan limits)?
```

## Adding a result to the table

1. Paste the returned row into the [capability table](#capability-table). Tag every cell from the answer as `[self <date>]`.
2. Replace a `[self]` tag with `[run <date>]` only after you see it work (for example, a real `p-999k_read` of a synthetic message).
3. Apply the [triage rules](architecture.md#triage-rules) to choose the adapter type (A, B, C, or D). If a deciding cell is `unknown`, triage again before building.
4. Write `<home>/triage/<agent>.json` (the default home is `.local`). Make sure that the file matches [the triage schema](../schemas/triage.schema.json). The file must not contain tokens or live hosts. See [Triage file to init](architecture.md#triage-file-to-init).
5. Add a dated entry to the [research log](research.md#adding-a-probe) with what passed, what failed, and what wasn't tested.
6. Update the [deployment matrix](#deployment-matrix) and the README's agent table.

After you choose a type, paste the matching setup skill from [setup skills per adapter type](setup-skills.md) into the agent.

Keep tokens, live endpoints, and personal paths out of this page.
