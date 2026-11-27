# Troubleshooting

Use the same executable and private `CLOUD_HOME` from the [quickstart](quickstart.md#1-install-the-cli).
If a setup command fails, read its terminal error first. If `run` started, check the loopback hub:

```sh
"$RADIO999K_BIN" health --home "$CLOUD_HOME"
```

If the hub is not ready, check `$CLOUD_HOME/pigeonmq-cloud-state.json` for the runner's `reason`.
If the hub is ready but the agent cannot connect, check [the tunnel](#through-the-tunnel) separately.

## Older queue databases

This release opens schema 4 only. If `init` or startup rejects an existing queue, check its version without changing it:

```sh
sqlite3 -readonly "$CLOUD_HOME/pigeonmq-cloud-queue.sqlite3" 'PRAGMA user_version;'
```

If it reports `2`, do not edit the version number. This release has no supported converter.

- To keep the old state, stay on the release that supports the old queue.
- To start empty, stop the old runner and back up its home. Keep the original home. Choose a different private home and follow the [quickstart](quickstart.md#1-install-the-cli). The new home does not carry over messages, ACKs, or Events state.

## Runner (`run`)

| You see | Meaning | Fix |
| --- | --- | --- |
| `cloud_adapter_policy_required` | No policy file in the home | Run `setup-muse`, or add a policy ([configuration](configuration.md#policy-format)) |
| `remove_legacy_senders_from_policy` | Muse or Grok Bot policy still contains the retired `senders` field | [Remove only that field](configuration.md#policy-format). Do not change the registry or credentials. |
| `cloud_credential_provision_failed` | Couldn't create or read the Keychain token | Check Keychain access. The policy must say `"keychain"` or hold a digest |
| `policy_or_existing_queue_unavailable` | Muse storage not prepared or policy invalid | Re-run `setup-muse` with the same home and peer |
| `grok_policy_or_existing_queue_unavailable` | Grok Bot storage not prepared | Run `grok-prepare` |
| `codex_policy_or_existing_queue_unavailable` | Codex policy, registration, or queue is invalid | Check the private standard files in this home |
| `codex_events_storage_unprepared` | Events settings lack prepared storage | Run `codex-prepare` with the same queue, policy, and registration |
| `queue_identity_changed_restart_required` | Queue file changed during hub recovery | Review the replacement, then restart `run` |
| `recovering` | Hub exited and the runner is restarting it | Check repeated failures. The queue and Events outbox remain on disk. |
| `local_ready` | Local hub is running without a managed public tunnel | Check your external proxy or tunnel and any Cloud subscription separately |
| `tunnel_mode_required` | `run` started with no tunnel mode | Set `RADIO999K_TUNNEL=external` and `RADIO999K_PUBLIC_URL` for your own domain, or `RADIO999K_TUNNEL=tunnl` for testing. Use `--local-only` for loopback tests only. |
| `tunnel_env_file_unavailable` | Cannot read the selected private env file | Check the file path and owner-only permissions on the file and its directory |
| `invalid_tunnel_env_file` or `tunnel_env_file_requires_mode` | Invalid assignment or missing tunnel mode in the file | Follow the [private file format](tunnels.md#use-a-private-env-file) |
| `ssh_auth_or_host_key_rejected` | SSH key refused or host key changed | Check `pigeonmq-cloud-tunnel-ed25519` and the pinned host key. The runner won't retry this. |
| `transient_tunnel_start`, repeating | No URL within 30 s. Retrying every second | Network or provider issue. A provider can reject port forwarding after key auth succeeds. The reason is not shown. |
| `unexpected_stable_url` | The tunnl.gg runner did not report a `https://*.tunnl.gg/` origin | Check the tunnl.gg provider output |
| `cloud_runner_owned_or_home_unavailable` | Another `run` owns this home | Use that runner, or stop it first |
| `generic_policy_absolute_path_required` | `--auth-policy` given as a relative path | Use an absolute path |

## HTTP responses from the hub

| Status / error | Meaning |
| --- | --- |
| `401` | Missing or wrong Bearer token, or token revoked |
| `403` | Check the mailbox grant and the selected operation. `p-999k_recipients` needs `send`. For a protected Dot or Codex route, also verify the Cloudflare Access assertion and binding. |
| `404 profile_disabled` | That app's policy file isn't loaded. Restart `run` after adding it. |
| `404 route_not_found` | Wrong path. Use the selected profile's `/muse/mcp`, `/grok/mcp`, `/dot/mcp`, or `/codex/mcp`. |
| `405 post_required` | Only POST is accepted (a browser GET gets this) |
| `406` on a Streamable HTTP endpoint | Send `Accept: application/json, text/event-stream`. |
| `409 request_already_active` | A Muse or Grok tool request with that ID is already in flight, or too many are active. |
| `413 body_too_large` | Request over the route's size limit |
| `reply_conflict` | Already answered with a different reply. Don't reply again. |

## Through the tunnel

| Symptom | Likely cause |
| --- | --- |
| 403 or a Cloudflare 1010 page before reaching the hub | The client sent no or an unusual `User-Agent`. Send a normal one. Python `urllib` defaults have hit this. |
| Browser shows a warning page | Provider interstitial for browsers. Agents should use JSON-RPC POST. |
| Works locally, but the connector fails | Check that the state is `connected` and compare the connector origin with `public_origin`. If the origin changed, update every agent's endpoint ([Tunnel URLs](tunnels.md)). A stale origin does not imply a particular HTTP status. |

## Events callbacks

For Dot and Codex Cloud Events, look for JSON warnings on hub stderr with `component: "events_callback"`. `retry_scheduled` marks a retryable failure on the first attempt, without warnings for intermediate retries. `rejected` means the result was not retryable. `retry_exhausted` means the last allowed attempt failed. `reservation_exhausted` counts final reservations that expired without a recorded result.

`decision_status` is the numeric retry decision code. A transport failure can use a local code without any HTTP response. These warnings contain no callback URL, credential, message ID, or payload. They do not indicate mailbox ACK or model execution.

## Grok Bot wake

`$CLOUD_HOME/grok-wake-status.json`:

| `state` | Meaning | Fix |
| --- | --- | --- |
| `running` | Normal | — |
| `auth_blocked`, `detail.reason` = `keychain_unavailable` | The notifier could not read a valid Keychain item at startup. The hub stays online. | Check access under the hub's macOS user and process. Restart the hub after you fix access. |
| `auth_blocked` | The webhook returned 401 or 403, or file credentials failed validation. | In Keychain mode, fix the credential or access. Then restart the hub. In file mode, fix the two credential files. The notifier resumes after they change. |
| `rate_limited` | Webhook returned 429. Paused 5 minutes. | Wait. Also check routine run limits. |
| `failed` | Notifier couldn't start. The hub still serves. | See `detail.reason` |
| `failed`, `detail.reason` = `unsafe_lock_file` | The lock file `pigeonmq-cloud-queue.grok-wake.lock` next to the queue is not a private regular file (mode 0600, owned by you, one link) | Set it to mode 600, then restart `run`. Older builds created it as 0644. |
| `failed`, `detail.reason` = `already_running` | Another notifier holds the lock for this queue | Run only one hub per queue. |
| file missing | Wake did not start. | Enable the Grok policy. At hub startup, the Keychain marker or both private files must exist. |

Agent woken twice for one message? Two notifiers are pointed at the same webhook, or two hubs share it. Give each hub its own webhook, and have the agent handle each `message_id` once.

Agent woken but `p-999k_read` says not found? The wake came from a different hub than the one the agent reads from. One webhook per hub.

## Check a hosted address

On the hub Mac, run `999k hosted-status --home DIR` with the hub home used to
reserve the address. If the reservation stopped waiting, repeat
[Reserve a hosted address](orca.md#reserve-a-hosted-address) with that same home.
An unused address can expire if its tunnel never connects. If access expires
or is revoked, contact the operator who provided it. Your queue and mailbox
credentials stay on your Mac.

If setup finds a saved connection without its registration proof, restore
`hosted-enrollment.json` from a private backup of the same hub home. Then
repeat setup in that home to reuse the address.

## Check a pending Orca reply

If Orca already reported **Queued**, ask the relay to check the same request.
Use the original request ID if you have it. The generated relay can schedule
up to three checks before it stops waiting.

If you have the original 999K ID and need to check the hub, run your peer's
setup command with `--orca-proof-original <original-id>`. Use the same hub
home and sender. The command checks for the linked reply without sending a
new request. If it reports a verified reply, check the originating project
chat for delivery. A pending or invalid original returns a failure status.

## Refresh a saved MCP app

If the serving hub advertises a tool that the saved app cannot call, refresh
the app’s tool list before creating another connector.

1. After upgrading the hub, run `tools/list` against the exact route with its
   existing authentication. For example, check that `p-999k_inbox` advertises
   `scope`, `query`, and `order` when testing inbox search. If the serving hub
   lists an older schema, check the binary and home before you refresh the app.
2. In the intended ChatGPT account or workspace, open the saved app and
   choose **Refresh** if offered. Keep the same app and Events subscription.
3. In the original conversation, call the tool with a synthetic test message.
   If the conversation still uses an older schema, retry with the same app in
   a new conversation.
