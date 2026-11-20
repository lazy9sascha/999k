# One LaunchAgent for a private hub

Use this procedure for an existing shared hub with private Dot or Codex Cloud tunnels, an email relay, or both. One macOS LaunchAgent runs `pigeonmq run`. The runner owns the same loopback hub and queue. It starts the selected companions only after the hub reports its profiles, owns the port, and responds to a protocol probe.

A failed companion retries without replacing the hub or its siblings. The retry delay grows to at most 30 seconds. When the hub stops, the runner stops and reaps the companions before it stops a still-running hub. If the hub must restart, the runner checks the queue identity and proves readiness again before it starts the companions.

This mode does not install a service or migrate an existing service. It does not select a public URL or change the separate `init --apply` workflow. Do not combine the two workflows on the same home or port. Existing public external and SSH modes do not change.

Updated hub binaries use one owner-private `.hub.lock` file next to the canonical SQLite queue. Another updated hub cannot open a listener for that queue, even on a different port. Do not delete the lock file or move the queue while a hub runs. Old binaries ignore the lock. Do not run them in parallel with the new hub.

## Prepare the files

1. Copy the [manifest example](../examples/launchd/companions.example.json) to the existing private hub home. Set mode 600 for the file and 700 for its parent. Keep it out of Git. Set `version` to 1. Run `stat -f '%d %i' <hub-home>/pigeonmq-cloud-queue.sqlite3`. Copy the two decimal numbers to `queue_identity.device` and `queue_identity.inode`, in that order. Omit unused roles. Select at least one role.
2. For each tunnel, use the approved installed client or its existing exec wrapper. Copy the existing arguments, starting with `run`. Use one JSON string for each argument. There is no shell expansion. Set `working_directory` and `home` to the existing private absolute paths. Set `path` to the existing `PATH` entries, in order, as separate absolute paths. The runner supplies only `HOME` and `PATH` to the tunnel. Keep the same tunnel ID, runtime config, header sources, and authentication settings. Use absolute `file:` references for runtime keys and header values, not inline values. Set `--mcp.server-url` to the exact hub loopback port and selected route. For health, use `127.0.0.1:0` and a different private URL file for each tunnel.
3. For the email relay, copy the existing absolute registration, cursor state, and provider command paths. Keep the same mailbox names and interval. The runner supplies the queue path from `--home`. This mode preserves the static `--mailbox` relay. It does not convert a managed `--config` relay or cursor.
4. Copy the [LaunchAgent example](../examples/launchd/pigeonmq.private-supervisor.plist). Replace the binary, home, port, and manifest paths. Use a durable installed binary outside the build directory. If the old runner uses bundled profiles, remove both `--profiles-file` and its path. Keep all programs in the foreground. A companion and its descendants must not leave their process group. Run `plutil -lint` on the copy.

`--companions-file` selects private companion mode. Do not pass `--env-file` or `--local-only` with it. This mode does not use the public tunnel environment variables. Do not point a trial at an active hub, its queue, or its tunnels.

## Replace old jobs during maintenance

1. Record the exact job labels, their loaded and disabled states, and the device and inode of the existing queue. Keep the old installed binary and private settings. Check which jobs use this hub. Do not include other hubs.
2. Stop the old reconnect job first. Stop the old tunnel clients and relay, then stop the old hub. Disable old restart paths. Move the old plist files to an owner-private archive outside `~/Library/LaunchAgents`. Unloading alone does not prevent their return at login. Verify those jobs and their children are gone. Verify no other listener uses this queue, including on a different port.
3. Place the new plist in `~/Library/LaunchAgents`. Use `launchctl bootstrap "gui/$(id -u)" <new-plist>` once. Keep the same home, port, queue, and route bindings. Do not run setup or reissue credentials.
4. Run `pigeonmq health --home <hub-home>`. Check that the hub is ready and the queue device and inode still match. For each tunnel, check the current client's local listener and then its `/readyz`. Match the listener PID to that role's current PID before using its health URL file. An old file or a running PID does not prove readiness. Use a read-only call from each intended app to check its mailbox access. Check relay delivery and Events separately. A healthy hub does not prove either.
5. To roll back, run `launchctl bootout "gui/$(id -u)/pigeonmq.private-supervisor"`. Archive the new plist outside `~/Library/LaunchAgents`. Verify its processes are gone and cannot restart before you restore the old plists and their saved disabled states. Restore the old installed binary and settings only if needed. Use the same current queue and relay cursor. Do not overwrite either with an older copy.

A queue identity mismatch prevents startup after a crash or login. If you intentionally move or restore the queue, first stop this LaunchAgent. Verify the intended queue and its current identity before you update the private manifest and restart. Do not update the identity to clear an unexplained mismatch.

The health report shows each selected role as `starting`, `running`, or `retrying`. `running` proves startup only. The health command exit code describes the hub and queue. It does not describe the tunnels or email delivery.
