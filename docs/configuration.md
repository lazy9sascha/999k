# Configuration

Everything lives in one private runtime folder (the "home"). Pass it with `--home`. Keep it outside the repository. The folder must be owned by you with no group/other access, and files are mode 600.

## Commands

Run `pigeonmq <command> --help` for exact flags.

### Everyday

| Command | What it does |
| --- | --- |
| `init --home DIR` | Prepare the home and missing connector drafts. Scan system and user Applications folders. |
| `init <agent> --home DIR` | Stage a [built-in profile](#manage-built-in-profiles) or a [dedicated local mailbox](#stage-local-mailboxes). |
| `init <agent> --home DIR --apply` | Apply a staged built-in profile on macOS or activate a dedicated local replacement. |
| `remove <agent> --home DIR --apply` | Retire a managed built-in profile and preserve its queue rows. |
| `discover --home DIR` | Same detection. Only creates settings that are missing. Detection means "app bundle found", not "connected". |
| `setup-muse --home DIR --peer NAME` | Muse in one step: queue, registration (`muse` + peer), policy, Keychain token. Safe to repeat. |
| `setup-codex --home DIR --peer NAME [--callback-host HOST --resolver PATH]` | Prepare the Codex Cloud queue, registration, policy, Keychain token, and optional Events storage. |
| `setup-grok-wake --home DIR [--from-private-files HOST] [--stage-only]` | Save the wake URL and Authorization value in one Keychain item. Use hidden input or import two existing private files for the specified HTTPS host. |
| `run --home DIR [--port N] [--env-file PATH] [--profiles-file PATH] [--local-only]` | Supervise the configured hub. A tunnel mode is required. See [tunnel settings](tunnels.md) for your own HTTPS origin, optional tunnl.gg, or loopback testing. |
| `handoff --home DIR --app muse\|grok-bot\|dot [--copy-token]` | Copy a connector prompt after a connected tunnl.gg tunnel, or explicitly copy the Bearer token from Keychain. |
| `credentials --home DIR --app APP status` | Check local token custody and its policy digest. Does not prove agent use. |
| `credentials --home DIR --app APP import --token-file PATH` | Recover an existing matching token from a private file. Keeps the policy unchanged. |
| `credentials --home DIR --app APP reissue --token-file PATH [--use-stored-token]` | Explicit token repair. Save a private repair token and change only its policy digest. |
| `credentials --home DIR --app APP export --token-file PATH [--format token\|authorization-header]` | Create a private credential file for an explicit client reference. |
| `dot-tunnel-status --home DIR` | Show the local Dot tunnel state. |
| `health --home DIR` | Check whether the supervised loopback hub answers. |
| `dlq-inspect --queue PATH --id ID` | Inspect one quarantined request without writing. See [dead-letter inspection](#dead-letter-inspection). |

### Lower level

| Command | What it does |
| --- | --- |
| `serve-hub --queue DB --port N --profiles-file PATH --continuous` | Serve the selected profiles on loopback. See [select existing hub profiles](#select-existing-hub-profiles). |
| `serve-hub --mailbox-home DIR --port N [--seconds N]` | Serve only the isolated mailboxes staged by `init <agent>`. See [local mailbox setup](#stage-local-mailboxes). |
| `muse-prepare` / `grok-prepare` / `dot-prepare --queue DB --auth-policy P` | Prepare adapter storage in an existing queue. |
| `codex-prepare --queue DB --auth-policy P --registration R` | Prepare storage for the Codex Events route ([architecture](architecture.md#codex-mcp-and-events)). |
| `muse-check --queue DB --auth-policy P` | Check that Muse storage is prepared. |
| `codex-queue --id ID --sender S claim\|complete` | Historical app relay seam. The native local worker uses `/receiver`. |

The optional generic `/mcp` route requires a JSON object for tool arguments. Use named arguments, not a positional array.

Pass `--db` to `codex-queue` to select its queue database.

### Replacements for retired CLI commands

Use a Bearer token for a profile or API that already allows the operation.

| Old command | Use instead |
| --- | --- |
| `send` | Call `pigeonmq_send` on the sender's MCP profile with `recipient`, `body`, and `idempotency_key`. Keep the same key on retry. If the generic API is already enabled with `--auth-policy`, scripts can use `POST /messages/send` with `sender`, `recipient`, and `body`. |
| `reply` to an unclaimed request | Call `pigeonmq_reply` on the recipient's MCP profile with `message_id` and `body`. It stores the linked reply and ACKs the original in one transaction. |
| `reply --receipt` for a leased request | Keep the worker's authenticated [`/receiver` completion path](#local-codex-receiver). `pigeonmq_reply` does not accept a receipt or replace leased completion. |

## Select existing hub profiles

By default, `run --home DIR` uses the conventional policy files in the Cloud home. To choose profiles with specific routes and policy files, pass `--profiles-file PATH`. Use absolute paths for PATH and the Cloud home. The manifest must be an owner-private regular file (mode 600 or 400, for example). It shares the 16 KiB size limit used for private policies and registration.

```json
{
  "profiles": [
    {"adapter":"muse","route":"/team-muse/mcp","policy":"/absolute/path/to/approved-policy.json"},
    {"adapter":"grok-bot","route":"/grok/mcp","policy":"pigeonmq-cloud-grok-bot-policy.json","wake_files_dir":"/absolute/private/wake-files"},
    {"adapter":"codex-local","route":"/receiver","policy":"pigeonmq-cloud-codex-local-policy.json"}
  ]
}
```

Valid adapter IDs are `muse`, `dot`, `grok-bot`, `codex-cloud`, `codex-local`, `cue`, and `instinct`. Use one entry per adapter, one route per entry, and a distinct private policy file. In `policy`, use a filename at the top of the Cloud home or the absolute path to an existing file, including one outside the Cloud home. Relative subpaths and `.` or `..` path segments are not allowed. The policy file and its containing directory must be owner-private and must not be symlinks. An MCP route can use a lowercase slug in `/<slug>/mcp`. The native receiver keeps `/receiver`. A route reserved for another adapter cannot be reused.

The hub uses each mounted route exactly as written. The manifest mounts only the listed profiles. A renamed route does not leave an alias at the default route. A missing or invalid policy stops startup instead of falling back to a conventional file. The running supervisor pins the manifest contents when it starts. Restart the supervisor to apply a deliberate change. Use the existing Bearer policy and registration rules. The file cannot set credentials, grant access, add a new adapter, or start the hub by itself.

Only the selected Grok Bot profile can set optional `wake_files_dir`. Use an absolute, owner-private directory with no symlink at that path. It must contain regular, owner-owned `grok-webhook-url` and `grok-webhook-auth` files with mode 600, one hard link each, and no more than 4 KiB each. Do not put either value in the manifest. Startup rejects an invalid source or a `grok-webhook-keychain` marker in the Cloud home. It does not fall back. The notifier reopens the selected files for each send. The queue, wake status, state, and lock stay in the Cloud home. Omit the field to keep conventional Keychain or home-file discovery.

The low-level `serve-hub` command can also take `--profiles-file PATH` with the absolute queue path. Do not combine it with explicit per-profile policy or registration flags. Setup and credential commands still use conventional filenames. Set a remapped connector route manually after checking it with its own bearer.

## Manage built-in profiles

Install the release executable at a stable absolute path outside the checkout and temporary directories.
See [Build](quickstart.md#1-build) for an owner-private install path.
Use `init --home DIR` to prepare an absolute owner-private home for a new installation.
Use `init NAME --home DIR` to stage a profile in that home.
Run the installed executable with `init NAME --home DIR --apply` to start or reload the managed hub.
A successful apply checks authenticated route readiness on loopback.
Set `--port` at first staging to select a port other than 54843.

Each macOS login uses one managed home and one fixed pair of labels, `pigeonmq.hub` and `pigeonmq.email-relay`.
The hub binds only to loopback. Apply and remove all profiles for that login against the same home.

Cue and Instinct can stage a protected Bearer on a new installation.
Dot, Codex Cloud, Muse, and Grok Bot need a prepared connector policy and registration.
Use the existing adapter setup and the exact active manifest with `--profiles-file PATH` when applicable.
Named init reuses the selected policy and credential for an already configured binding.
Review a settings conflict before applying.

A schema 4 queue without the generation catalog supports read-only recognition of compatible existing profiles.
It rejects activation or removal through the managed commands.
Existing connector setup and the existing supervisor remain appropriate for that queue.

Use `remove NAME --home DIR` to preview a managed removal.
Add `--apply` to revoke the registration and retire its generation. Queue rows remain stored.
To reconnect the same name, stage with `init` and apply the staged Bearer.
Reconfigure the app with the new Bearer. Earlier messages remain outside the replacement mailbox.

### Opt in to email arrivals

Provide a valid address for Cue or Instinct with `--email-recipient` when staging.
On first email setup pass `--relay-command` with an absolute path to an owner-private executable.
The command receives two arguments, the email destination and the PigeonMQ message ID.
Use an absolute interpreter path in scripts and absolute paths to their dependencies.
The managed relay supplies a restricted system `PATH`.
Configure the command using your existing provider login and use the ID as its idempotency key.
The provider must suppress repeat sends for at least 24 hours.

Email membership is stored in an owner-private configuration file.
The `pigeonmq.email-relay` service is created for the first selected mailbox.
Adding or removing an email mailbox while another remains preserves the same plist and running relay.
Removing the final email mailbox stops and removes the relay job. The hub plist remains.
At login the hub can run with an empty route set until a profile is applied.
Email sends start at the recorded activation boundary.

## Stage local mailboxes

Use a new owner-private home for synthetic data. PigeonMQ creates the home if its parent exists. Use an absolute path.

```sh
pigeonmq init synthetic-pull --home "$TEST_HOME"
pigeonmq init vooy --home "$TEST_HOME"
pigeonmq serve-hub --mailbox-home "$TEST_HOME" --port 54925 --seconds 60
```

Each init prints JSON with `mailbox`, `route`, `token_file`, `created`, `runtime`, and `remote_verified`. It reports the token file path, not the token. The token file has mode 0600. Running init again checks the existing configuration and token without replacing them. Keep this home and its tokens out of Git.

The same binary serves any valid staged name through the official Rust MCP SDK (rmcp). For a local test, use POST with `Content-Type: application/json` and `Accept: application/json, text/event-stream` on the exact route printed by init. Each mailbox has its own policy and registration and registers no peers. A token for one route cannot access the other. To stage from triage, save a [valid local record](architecture.md#triage-file-to-init) as `$TEST_HOME/triage/<agent>.json` before init. The record selects read, wait, or both. Without a record, a new mailbox offers both. Do not use the checked-in Vooy candidate as a local record: it describes an unverified remote app.

This command does not connect the real Vooy app, configure a connector, or start a tunnel. Do not forward this test listener through a tunnel. The normal runner does not load these routes. Start a new bounded listener to apply new registrations.

### Replace a dedicated local mailbox

Run these commands against the same dedicated home:

```sh
pigeonmq remove my-agent --home "$MAILBOX_HOME"
pigeonmq init my-agent --home "$MAILBOX_HOME"
pigeonmq init my-agent --home "$MAILBOX_HOME" --apply
```

Removal revokes the existing binding and preserves its queue records.
The next `init` stages a new Bearer and storage identity for the same route.
The staged replacement stays inactive until `--apply`. Repeat `init` to reuse the staged credential.
Restart a dedicated listener to mount the active replacement.
Use the new protected Bearer. The retired listener binding rejects requests.
These commands apply to dedicated local mailboxes. Built-in cloud profiles use their own setup and deployment procedures.

Queues created by this release include the generation catalog and retain schema 4.
A legacy queue without that catalog can continue serving existing bindings.
Removal and same-name replacement fail closed on that queue.
Use a generation-aware build for every process serving a queue with replaced mailboxes.

## Dot through a private Secure MCP Tunnel

Use a separate tunnel for this Dot profile. Keep existing agent tunnels unchanged.
This procedure requires a prepared queue, an active `dot` registration, and a matching Dot policy.
Keep the policy and both credentials outside Git. Use owner-only directories and mode 600 credential files.

1. Prepare the Dot storage with `pigeonmq dot-prepare --queue "$QUEUE" --auth-policy "$DOT_POLICY"`.
2. Use the existing hub if it serves the Dot route. For a dedicated listener, set `PROFILES_FILE` to the absolute path of a private Dot-only [profile manifest](#select-existing-hub-profiles).
   Use adapter `dot`, route `/dot/mcp`, and the absolute `DOT_POLICY` path in its `policy` field.

```sh
pigeonmq serve-hub --queue "$QUEUE" --profiles-file "$PROFILES_FILE" \
  --port "$DOT_PORT" --continuous
```

3. Start the official tunnel client with the approved tunnel and protected file references.

```sh
tunnel-client run \
  --control-plane.tunnel-id "$DOT_TUNNEL_ID" \
  --control-plane.api-key "file:$RUNTIME_KEY_FILE" \
  --mcp.server-url "http://127.0.0.1:$DOT_PORT/dot/mcp" \
  --mcp.extra-headers "Authorization: file:$DOT_HEADER_FILE" \
  --mcp.discovery-extra-headers "Authorization: file:$DOT_HEADER_FILE" \
  --mcp.oauth-discovery disabled \
  --health.listen-addr 127.0.0.1:0 \
  --health.url-file "$HEALTH_URL_FILE"
```

Use absolute file paths. The header file contains the complete `Bearer TOKEN` value.
The runtime key authenticates the tunnel client, not the Dot mailbox.
The Dot header authenticates discovery, tool calls, and Events at the hub.
The route uses rmcp Streamable HTTP. Clients must send `Accept: application/json, text/event-stream` and use string or signed 64-bit integer request IDs.
These commands remain active. They do not install automatic startup after a restart.

4. Check the tunnel readiness endpoint. Local readiness alone does not prove access from ChatGPT.
5. Open the private custom MCP form in the intended personal ChatGPT account.
6. Select **Tunnel**. Enter the approved tunnel ID.
7. Select **None** only for this private connection with the enforced upstream Bearer header.
8. Accept the authorized risk notice. Create the plugin. Connect the plugin to the personal account.
9. Check that ChatGPT lists the plugin as installed.
10. Use the new plugin from the intended Dot to call `pigeonmq_recipients`.

**None** removes additional OAuth at the ChatGPT layer. It does not remove hub authentication.
Each authorized caller through this tunnel uses the Dot identity. Do not share this connection with another agent identity.
Do not publish the plugin or grant another workspace access without separate approval.

Record local readiness, installation, and the actual Dot call as separate results.
A successful recipients call proves read-only access. It does not prove message delivery, replies, or Events.
A recipient status of `unknown` does not mean that the tunnel failed.
Codex Cloud Events and a shared tunnel need separate configuration and acceptance.

See the official [tunnel configuration](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md)
and [ChatGPT connection guide](https://developers.openai.com/plugins/deploy/connect-chatgpt).

### Optional local arrival notifier

Run `pigeonmq notify-arrivals` as a separate service to notify selected registered mailboxes.
It uses the existing queue in read-only mode. First start records the current sequence.
Later sweeps observe new eligible ordinary messages and linked terminal replies.

Set `--queue`, `--registration`, `--state`, and `--command` to absolute paths.
Set `--mailbox` once per selected mailbox. Use an owner-private directory for the notifier state.
The command receives two arguments in order: the mailbox and message ID.
Keep the command owner-private and executable. Use the message ID as the recipient provider's idempotency key.
The provider must suppress repeat sends for at least 24 hours.
The notifier retries an uncertain delivery for up to 20 hours and reports expired reservations.
Set `--interval-secs` for a long-running local service.

### Optional Dot mailbox Events

The Dot profile can send signed callbacks for ordinary messages and linked terminal replies.
It delivers eligible messages that arrived after the subscription became active.
Callbacks carry identifiers only. Read the message through the authenticated mailbox.
The Dot profile disables this feature when its protected policy has no `events` field.
It uses the existing `/dot/mcp` route, queue, subscriptions, and durable outbox.
It does not use a scheduled mailbox check in ChatGPT.

First check for a supported platform subscription procedure for the installed private app.
Get the callback host from that procedure or its official documentation. Do not guess the host.
Plugin refresh alone does not prove platform support or a callable Dot conversation.

Add these settings only after approval for callback delivery:

```json
"events": {
  "callback_hosts": ["<authorized-platform-callback-host>"],
  "resolver": "/absolute/path/to/pigeonmq-dot-resolve"
}
```

Preserve an immutable tested binary and a working rollback binary before maintenance.
Do not use a build directory as the installed binary location.

1. Stop only the dedicated Dot listener during an approved maintenance interval.
2. Run `dot-prepare` with the same queue and protected Dot policy.
3. Start the updated Dot listener with its existing transport and Bearer authentication.
4. Refresh the installed plugin discovery in the intended account.
5. Bind an expiring platform subscription to the intended Dot conversation with recipient `dot`.
6. Check a separately authorized reply through that conversation.

Discovery advertises `events/list`, `events/subscribe`, and `events/unsubscribe` through MCP Events.
The subscription owner is the authenticated Dot principal. A caller cannot select another recipient.
The worker checks current registration and policy before subscription changes and callback delivery.
It uses the Dot instance lock. Codex keeps its separate policy and lock.

Callbacks contain message identifiers. Linked terminal callbacks also include `in_reply_to` and `terminal: true`.
The first successful subscription excludes messages that already exist in the queue.
The server stores the queue sequence boundary in the same transaction as the grant.
Active refresh and restart preserve that boundary. The caller cannot request replay or select a cutoff.
A subscription after expiry or unsubscribe starts with a new boundary.
Old replies remain available through the mailbox. The worker does not acknowledge them.
A different callback URL creates a different subscription with a new boundary.
Delivery success changes only the durable outbox. It does not acknowledge or reply to the message.
The Dot must read the message through its authenticated mailbox before reporting the result.
The Dot must not answer a terminal reply as a new request.

Offline tests prove discovery, authentication, recipient isolation, signed metadata, and persistence after restart.
They do not prove installation, a live Dot subscription, or a platform wake.
Separate live checks covered terminal-reply Events and exact authenticated reads without polling ([DOT-03](research.md#dot-03-standalone-terminal-reply-events)).

## Codex Cloud operation

Choose mailbox-only operation or Events when you first prepare the profile.
Mailbox-only setup does not start a model or require Events storage:

```sh
pigeonmq setup-codex --home "$CLOUD_HOME" --peer my-peer
```

For Events, supply the authorized callback host and the installed resolver's absolute path:

```sh
pigeonmq setup-codex --home "$CLOUD_HOME" --peer my-peer \
  --callback-host '<authorized-callback-host>' \
  --resolver /absolute/path/to/pigeonmq-dot-resolve
```

Repeat `--callback-host` for each authorized host. Never accept a host from an untrusted message.
Setup prepares storage explicitly. The listener never creates or migrates it.
Existing policy grants remain unchanged. Review conflicts rather than overwrite an existing profile.
The Codex MCP `send` operation uses active registration to select recipients. Setup leaves `send_to` empty because it configures Codex MCP, not the generic routes.

Use the existing hub if it serves the Codex route. If you add this profile to a shared hub, load it with the hub's existing startup configuration.
The Secure MCP Tunnel client connects to the loopback route. It needs no public hub URL.
Ignore `run_command` and `run_arguments` in older Codex connector drafts. The hub operator selects the startup settings.

For a new, dedicated private hub, save this [profile manifest](#select-existing-hub-profiles) as a mode-600 file in the private home:

```json
{"profiles":[{"adapter":"codex-cloud","route":"/codex/mcp","policy":"pigeonmq-cloud-codex-policy.json"}]}
```

Start one listener for that home. This example uses port 54843 and the filename `codex-profiles.json`:

```sh
pigeonmq serve-hub --queue "$CLOUD_HOME/pigeonmq-cloud-queue.sqlite3" \
  --profiles-file "$CLOUD_HOME/codex-profiles.json" --port 54843 --continuous
```

Use the listener's actual port and route in the tunnel client settings below.

Export the [protected Bearer header](#credential-custody-and-handoff) for `codex-cloud`.
Configure the official tunnel runtime with these installed CLI settings:

| Setting | Value |
| --- | --- |
| `--mcp.server-url` | `http://127.0.0.1:54843/codex/mcp` |
| `--mcp.extra-headers` | `Authorization: file:/absolute/private/path/pigeonmq-cloud-codex-header` |
| `--mcp.discovery-extra-headers` | The same header file reference |
| `--control-plane.tunnel-id` | The authorized existing tunnel ID |
| `--control-plane.api-key` | A separate runtime-key reference, such as `file:/absolute/private/path/runtime-key` |

Use the official tunnel-client runtime manager for its process lifecycle.
Check its process, local readiness, and control-plane polling separately. Local hub readiness establishes none of these.
Use your existing OS supervisor for unattended hub startup.

For automatic Events delivery, bind an authorized platform subscription to the intended callable Work Cloud conversation.
Use web Work or an app-created desktop Work conversation with Cloud selected. Codex Local is a different receiver.
Check the exact native event subscription for that conversation. A conversation ID alone does not establish the target or create a subscription.
Check arrival, authenticated read, final linked reply, and original ACK through that conversation.
A tool list or successful callback alone does not establish the full exchange.
Both Work Cloud surfaces completed automatic event runs and linked replies in [CODEX-08](research.md#codex-08-work-cloud-events-on-this-hub).
Native desktop UI clicks and unattended restart recovery were not tested.

## Local Codex receiver

Keep `codex-local` separate from the Cloud mailbox `codex-cloud`.

### Choose a local Codex session from Dot

Give each local session its own receiver token and mailbox, such as `codex-local1` or `codex-local2`.
Register each mailbox and grant its worker `receive` and `reply` for that mailbox. The workers share the hub's `/receiver` route.

In Dot, call `pigeonmq_recipients` to list registered mailboxes. Choose the session's exact mailbox name and pass it as `recipient` to `pigeonmq_send`.
A mailbox can appear in the list while its worker is offline.

### Configure the receiver

The local worker uses the common receiver API. It does not open the queue or run a Cloud conversation.

Select the `codex-local` adapter at `/receiver` in the [profile manifest](#select-existing-hub-profiles). Use the central registration or set the manifest's `registration` path.
The private receiver policy uses a `principals` array with `receive` and `reply` operations and the `codex-local` mailbox.
Register `codex-local` and its allowed peers in the supplied registration file. Keep token files private, outside Git.

The foreground binary requires `--state`, `--token-file`, and `--binding`:

```sh
pigeonmq-codex-receiver --state /private/path/receiver-state \
  --hub-port 54843 --token-file /private/path/receiver-token \
  --binding /private/path/codex-binding.json
```

The worker and hub default to port 54843. Pass `--hub-port` if the hub uses another port.
Without `--run`, it checks the protected binding only. It does not contact the hub or native endpoint.
A `configuration_valid` result does not establish token validity, native readiness, or a message exchange.

The private binding JSON contains `thread_id`, `endpoint`, `cwd`, and `registration`.
The three paths must be absolute. The thread must be the approved existing local owner.
Any active registered peer may send. The legacy `approved_inputs` field no longer restricts request bodies.

| Flag | Behavior |
| --- | --- |
| `--run` | Execute one request through the bound native thread. |
| `--run --serve` | Keep the foreground worker active for successive requests. |
| `--attach-saved` | With `--run`, explicitly attach the same saved thread ID. |
| `--codex-bin PATH` | Select the Codex CLI used for daemon recovery in serve mode. Defaults to `~/.local/bin/codex`. |
| `--set-unattended` | Separate setup action. Sets approval policy to `never` and a workspace-write sandbox limited to the bound directory, with network access disabled. Excludes `--run`. |

The worker checks the native binding before claiming. It renews leases during long turns.
Serve mode waits through temporary hub transport failures when no native input is pending.
It has no fixed turn-duration limit. Lease, binding, and authorization checks remain active.
Saved uncertain work requires reconciliation. It does not automatically submit that input again.
Reconciliation saves its history cursor between passes and restarts. Older work cannot fall permanently outside a fixed history window.
The command does not install a background service. Synthetic receiver fixtures do not establish native or Cloud acceptance.
See [Codex paths](architecture.md#codex-paths-and-shared-operations) for shared and adapter responsibilities.

## Files in the home

| File | Created by | Purpose |
| --- | --- | --- |
| `pigeonmq-cloud-queue.sqlite3` | `init`, `setup-muse` | The queue |
| `.env` (optional) | you | Private tunnel settings. Pass `--env-file`. The runner does not search for this file. See [a private .env file](tunnels.md#use-a-private-env-file). |
| `triage/<agent>.json` | you (triage skill) | Optional local triage record. `init <agent>` reads it when present. No tokens or live hosts. |
| `local-mailboxes/` | `init <agent>` | Isolated mailbox queue (`mailboxes.sqlite3`) and per-agent directories (policy, registration, mode 0600 `bearer-token`). The normal runner does not load these. |
| `pigeonmq-cloud-registration.json` | `init`, `setup-muse`, you | Known mailboxes and revocation |
| `pigeonmq-cloud-<app>-policy.json` | `setup-muse`, you | Per-app access policy. `<app>` = `muse`, `grok-bot`, `dot` |
| `pigeonmq-cloud-codex-policy.json` | `setup-codex`, you | Codex Cloud principals and optional Events settings |
| `pigeonmq-cloud-codex-local-policy.json` | you | Local receiver principals, separate from the Cloud profile |
| `pigeonmq-cloud-<app>-settings.json` | `init`, `discover` | Connector draft: name, route, Keychain item. No secrets. |
| `pigeonmq-cloud-tunnel-ed25519` (+ `.pub`) | you (`ssh-keygen`) | SSH key for the tunnel only |
| `pigeonmq-cloud-tunnel-known_hosts` | you | Pinned, verified tunnl.gg host key |
| `pigeonmq-cloud-state.json` | `run` | Runtime state, loopback port when running, public origin when known, and failure reason |
| `pigeonmq-cloud-run.lock` | `run` | One runner per home |
| `grok-webhook-url`, `grok-webhook-auth` | you | Grok Bot wake target and header |
| `grok-webhook-keychain` | `setup-grok-wake` | Selects the Keychain item. Contains no secrets. |
| `grok-wake-state.json`, `grok-wake-status.json` | hub | Wake progress (not an ACK) and visible status |

Keep the entire runtime home outside Git. `.gitignore` covers standard queue, policy, token, and tunnel files, but not every wake file.

Run `health --home DIR` against a running supervisor to check the current loopback HTTP response.
It prints JSON and returns exit code 0 when ready or 1 otherwise.
The check does not open the queue, read secrets, or contact a tunnel or app.
Check `grok-wake-status.json` separately for the notifier's reported state.
Grok wake and supervisor diagnostics use JSON on stderr. Runner failures can report child exit codes, signals, or system error categories without logging child output from the Muse check or SSH. Keep CLI output and private state separate from shared logs.

On macOS, `serve-hub` disables Keychain prompts for its process before workers start.
Interactive setup and credential commands keep their existing behavior.
If Keychain access needs interaction, the Grok notifier reports `auth_blocked` with
`keychain_unavailable`. The hub continues serving mail. The notifier does not retry
that startup read. Resolve the existing item's access in macOS, then restart the hub.
The hub does not grant itself access, replace the item, or fall back to credential files.

For repeated deployments, keep the same code-signing identity and signing identifier.
Check each signed build and compare its designated requirement before replacement.
Ad-hoc signing ties the identity to one build. See [Apple's code-signing requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements/).
Stable signing does not grant access to an existing Keychain item. The owner must
authorize any new identity before unattended use.

## Dead-letter inspection

Run `pigeonmq dlq-inspect --queue PATH --id ID` on this Mac. Use the real path to a current owner-private queue, not a symlink. The command does not create, migrate, or repair a queue.

The command prints JSON. `dead_lettered` includes the attempt count and `dead_lettered_at_ms` (Unix time in milliseconds). `not_dead_lettered_or_missing` means there is no matching quarantined ordinary request. Both results exit with status 0. Read or validation errors exit nonzero.

The CLI output omits the body, receipt, and mailbox names. The command does not list, revive, or replay messages.

Use `POST /messages/dlq` on a hub with the optional generic `--auth-policy` and a real path to the queue. Send JSON with a Bearer token for a principal with the `inspect_dlq` operation. Only recipients in that principal's `mailboxes` are visible. Built-in agent profiles do not expose this route or an equivalent MCP tool.

Send `{}` for the first page. Each page has up to 50 `entries` in queue order and a `next_continuation` value. Send that value as `{"continuation":"VALUE"}` for the next page. A null `next_continuation` marks the last page.

Send `{"id":"MESSAGE_ID"}` for one entry instead of a page. Each entry contains only `id`, `state`, `attempts`, and `dead_lettered_at_ms`. For an exact ID, a missing, active, or out-of-scope message returns `{"state":"not_dead_lettered_or_missing"}`. The endpoint does not change the queue.

## Policy format

Muse and Grok Bot use the following mailbox policy shape.
Dot has a [separate policy](#dot-through-a-private-secure-mcp-tunnel). An enabled Dot policy gives its Bearer token access to the Dot mailbox tools. It has no per-operation switches or automatic expiry. Disable the policy or rotate the credential to stop using that token.

Codex Cloud, the common receiver, and the optional generic `--auth-policy` use a `principals` array. The generic `/messages/send`, `/rpc/request`, and their `/mcp` tools require both the operation and an exact recipient in `send_to`. An empty list grants no generic destinations. Generic routes do not consult registration. On `/codex/mcp`, the `send` operation and the active registration determine recipients. That route does not use `send_to`, even if you explicitly mount the same policy on both routes.

```json
{
  "principal": "muse",
  "mailbox": "muse",
  "token_sha256": "keychain",
  "operations": ["read", "send", "wait", "reply"],
  "enabled": true,
  "expires_at_ms": 18446744073709551615,
  "registration": "/absolute/path/to/home/pigeonmq-cloud-registration.json"
}
```

| Field | Meaning |
| --- | --- |
| `principal`, `mailbox` | Who the token belongs to and which mailbox it may use |
| `token_sha256` | SHA-256 hex of the Bearer token. Write `"keychain"` and the first `run` (or `setup-muse`) creates a token in the macOS Keychain and replaces this with its digest. Existing digests are never replaced. |
| `operations` | Any of `read`, `send`, `wait`, `reply`. `pigeonmq_recipients` needs `send`. |
| `enabled`, `expires_at_ms` | Off switch and expiry (milliseconds since epoch) |
| `registration` | Absolute path to the registration file |

For Muse and Grok Bot, the central registry determines inbound and outbound peer access. Each peer and the profile itself need an active registration with a mailbox that matches its identifier. Codex Cloud and Dot also use registration. Profiles that share a registration file share its active peer pool. The file does not grant peers separately for each profile.

Muse and Grok Bot reject the retired `senders` field. Before upgrading an existing hub, back up each owner-private policy and remove only its top-level `senders` field. Keep each token digest, operations, registry and owner-only file permissions unchanged. Check the updated policies with the existing hub before installing the strict build.

Bearer authentication, policy enablement, expiry, and operation checks still apply. These rules describe the current source. This documentation does not establish which binary an existing hub runs.

## Registration format

```json
{
  "muse":    { "mailbox": "muse",    "revoked": false },
  "my-peer": { "mailbox": "my-peer", "revoked": false }
}
```

Set `"revoked": true` to cut a mailbox off without deleting anything.

## Known agents: what setup really is

For an agent that has already been through [agent triage](architecture.md#agent-triage) (Grok Bot, Muse), setup is short:

| Side | Grok Bot (A. Wake + pull) | Muse (B. Pull only) |
| --- | --- | --- |
| Hub | Policy and registration ([below](#grok-bot-policy-by-hand)) | `setup-muse` |
| Wake notifier | `setup-grok-wake`: one Keychain item, or two private files when Keychain mode is not selected | None. Muse pulls. |
| Agent | The hub endpoint (`public_origin` + `/grok/mcp`) and its token | The hub endpoint (`public_origin` + `/muse/mcp`) and its token |

That's all: hub-side files, then endpoint + token on the agent side.

### Credential custody and handoff

Issuance, local custody, client configuration, and authenticated use are separate steps.
The `credentials` commands support `muse`, `dot`, `grok-bot`, `codex-cloud`, and `codex-local`.
They use the existing policy and the same home-specific Keychain account as provisioning.

Check the selected profile before a handoff:

```sh
pigeonmq credentials --home "$CLOUD_HOME" --app grok-bot status
```

The result reports `local_available`, `matches_policy`, and `agent_use: "unverified"`.
A stored policy digest does not prove that a matching token exists locally or at the agent.
If a matching token survives in a protected file, recover it explicitly:

```sh
pigeonmq credentials --home "$CLOUD_HOME" --app grok-bot import \
  --token-file /private/path/existing-token
```

The input must contain the raw token, with at most one final LF or CRLF.
Import rejects a digest mismatch or a different stored token. It never rotates the policy.
A digest cannot recover a lost token. If no plaintext copy survives, explicitly reissue it:

```sh
pigeonmq credentials --home "$CLOUD_HOME" --app grok-bot reissue \
  --token-file "$CLOUD_HOME/pigeonmq-cloud-grok-bot-token-repair"
```

Reissue saves the new token before inserting it into Keychain or changing the policy.
It changes only the selected digest. It preserves grants, registration, other principals, and Events settings.
If repair stops, repeat the same command with the same file. It reuses that token.
A different stored token or an incomplete repair file blocks repair. No stored Keychain token is overwritten.
Reissue invalidates the old client token after the policy update. Install the repair token through the client's secure credential field.
The result is `credential_reissued_pending_secure_field`. This command does not track client installation.

If local custody exists but its digest differs, choose the authoritative credential before repair.
To explicitly select the existing Keychain token, add `--use-stored-token`:

```sh
pigeonmq credentials --home "$CLOUD_HOME" --app grok-bot reissue \
  --token-file "$CLOUD_HOME/pigeonmq-cloud-grok-bot-token-repair" \
  --use-stored-token
```

This option preserves the stored token. It writes its protected handoff file and changes only the selected policy digest.
A conflicting handoff file still blocks repair. It never substitutes a different token silently.

`setup-muse` and `handoff` require matching local custody before reporting preparation or copying a setup prompt.
Neither command silently reissues a missing token. A matching token stays unchanged.

For a native receiver, export the raw token to its `--token-file` path.
For an official tunnel-client header reference, export the complete `Bearer TOKEN` value:

```sh
pigeonmq credentials --home "$CLOUD_HOME" --app codex-cloud export \
  --token-file "$CLOUD_HOME/pigeonmq-cloud-codex-header" \
  --format authorization-header
```

Paths must be absolute. Files must be regular, owner-private files with one link, inside an owner-private directory.
Export creates mode 600 files. It refuses a different existing file and never prints the token.
Use the header file for both tool and discovery authentication. Keep all credential files outside Git.

Muse and Grok Bot connectors still need their supported secure credential field.
`handoff --copy-token` reports `copied_pending_secure_field`. Clipboard copy or file export does not establish client configuration.
Check use through an authenticated request from the selected agent. A tool list establishes discovery only.

Use the supported secure credential field or an explicit protected file reference. Remote agents without either mechanism need a separate reviewed handoff design.

## Grok Bot policy by hand

If you already have the SHA-256 digest for your Grok Bot token, run the following command. It prepares the Grok Bot registration and policy. Register the intended peers, then continue at step 3.

```sh
pigeonmq setup-grok --home "$CLOUD_HOME" --token-sha256 "$TOKEN_SHA256"
```

For manual setup, start with a home that `init` or `setup-muse` created.

1. Add `"grok-bot": {"mailbox": "grok-bot", "revoked": false}` and each peer to `pigeonmq-cloud-registration.json`.
2. Create `pigeonmq-cloud-grok-bot-policy.json` with mode 600. Use the policy example above with `principal` and `mailbox` set to `grok-bot`.
3. `pigeonmq grok-prepare --queue "$CLOUD_HOME/pigeonmq-cloud-queue.sqlite3" --auth-policy "$CLOUD_HOME/pigeonmq-cloud-grok-bot-policy.json"`
4. Start the hub with the [tunnel settings](tunnels.md) for your connection.
5. For `external`, set the connector URL to your HTTPS origin plus `/grok/mcp`. Enter the matching Bearer token in the secure credential field.
   For a connected tunnl.gg test tunnel with a matching Keychain token, follow [Connect an app](quickstart.md#4-connect-an-app).

For direct HTTP calls, check the JSON-RPC `error` for request errors.
For a tool failure, check `result.isError` even when HTTP returns 200.
The first text item contains a JSON object with an `error` reason, such as `reply_conflict`.
HTTP authentication failures remain separate from tool results.

For the wake, see [quickstart step 5](quickstart.md#5-optional-grok-bot-wake).

## Local foreground MCP over stdio (preview)

Use stdio for a local MCP client that runs under the same OS user as your hub.
It needs no Cloudflare Tunnel or installation email. It uses the mailbox policy
already mounted in the hub. It does not create a queue, grant access, or start or
stop the hub. The preview applies only to a foreground hub started with
`serve-hub`. It does not enable stdio for an installed managed or hosted hub.

Add `--stdio-socket` to your existing `pigeonmq serve-hub` command. Keep the same
private queue and profile options. When the hub starts, it binds a private Unix socket
next to the queue. Do not run it alongside an installed managed service on the same queue. Run the
foreground preview with `serve-hub`.

Configure your client's stdio command with the same private hub home, a mounted
mailbox with its default route, and its existing owner-private bearer file:

```sh
pigeonmq stdio-mcp --home <hub-home> --mailbox <name> --bearer-file <private-file>
```

Give the client the bearer file path, not the bearer value. A missing or unauthenticated
hub returns a protocol error. The adapter does not fall back to TCP. Closing the client
leaves the hub running. This adapter handles one request at a time with the MCP 2025-03-26 initialization handshake. It does not expose remote events or cancel calls in progress.
