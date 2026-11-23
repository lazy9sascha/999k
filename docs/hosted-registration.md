# Hosted registration operator preview

The registration service manages the address and Cloudflare Tunnel for each approved installation. It listens on loopback. User Macs serve their own MCP endpoints and store their own queues.

## Prepare a private pilot

1. Create an owner-only directory with mode 0700 on the operator Mac.
2. Save the settings below in `settings.json` with mode 0600. Replace example values with the approved pilot settings.
3. Save the Cloudflare management token in `cloudflare.key` with mode 0600. Grant tunnel management for the operator account and DNS access for the selected zone.
4. Enable email sign-in in Supabase Auth. Configure the sign-in email to show the numeric code. Add each existing invited email and its Supabase account ID to `invited_accounts`.
5. Route the registration origin to the loopback service over HTTPS. Preserve the public registration Host header. Disable caching. Keep credentials and form bodies out of logs.

```json
{
  "registration_origin": "https://init.example.com",
  "supabase_url": "https://project.example.com",
  "supabase_publishable_key": "sb_publishable_example",
  "cloudflare_account_id": "<account-id>",
  "cloudflare_zone_id": "<zone-id>",
  "cloudflare_zone_name": "example.com",
  "hub_port": 54843,
  "mcp_path": "/muse/mcp"
}
```

Use the loopback port and MCP paths that the pilot installs on user Macs. Set
`"mcp_paths": ["/dot/mcp", "/codex/mcp"]` to publish both paths on the same
hostname. Use `mcp_path` for one path or include it alongside `mcp_paths` to
retain an existing path. Dot and Codex also get their exact path-scoped OAuth
resource metadata paths. Enable the matching profiles and auth on the hub.

```sh
cargo build --locked --bin pigeonmq-registration
./target/debug/pigeonmq-registration --home /path/to/private-registration-home --port 54900
```

The service creates `enrollments.sqlite3` in the private directory. It reserves an address before sign-in. After browser approval, it publishes only the configured MCP and discovery paths and returns the
installation's connector token. A successful finish on an existing installation
also reconciles its ingress with the current settings.

Open the private installation link from PigeonMQ. Enter your email address and the numeric email code. You do not need an existing Supabase Auth account. Older settings can still contain \`invited_accounts\`; the service warns that this field is ignored. Remove it. Do not enter a code from the Mac.
The link expires with the reservation. Open it only in the intended browser. Do not share or log the full link.
Opening the link does not send an email. The browser removes the private fragment and submits it only when you request the email code.
Supabase issues the numeric OTP for an email address, not an installation.
PigeonMQ checks that the verified account holds the exact email address for this attempt. It applies approval only to the installation bound to that attempt. The email code and the tunnel connector token do not grant MCP access. Existing MCP authentication does not change.
The request page does not show whether it sent an email. Response times can differ.
If an attempt fails, wait up to five minutes, then reopen registration from PigeonMQ. Use the same pending installation until its reservation expires.

## Reserve an address on your Mac

Use the HTTPS registration origin from your pilot operator. Keep the same private hub home on each run.

```sh
export PIGEONMQ_REGISTRATION_URL=https://init.example.com
pigeonmq init --reserve-address --home "$HOME/path/to/private-hub"
```

Approve the installation in your browser. If setup stops waiting or you close the terminal,
run the same command to resume. Do not copy the browser link or the installation files to a
different machine.

PigeonMQ reports `hosted_connector=staged` and `hosted_public_route=unverified`.
It saves the address and private connector credential in the owner-private hub home.
It does not create a mailbox, change a local hub, or start a connector. The registration
service can assign a loopback port and MCP paths, but those settings do not prove local
origin identity across restarts.

For a new pilot enrollment, \`--hosted-unix\` selects a private Unix socket and requires
an operator build that supports this mode. It has no TCP fallback. Do not switch an
existing enrollment to this mode.

Use the same \`--home\` and flags on each retry. If the home lacks the managed profiles
the operator selected, the credential stays staged. Apply the required profiles, then
run the same reserve command. The installer creates an owned LaunchAgent only if the
running managed hub answers on the private socket. The connector checks that socket
each time launchd starts it. An unrelated TCP listener cannot receive hosted requests.

\`hosted_connector=scheduled\` reports local setup. It does not prove the public route.
Retiring a managed profile that supplies a required path removes the hosted job.
It keeps the registration credential and does not revoke the public address.

### Connect Dot and Codex Cloud through Cloudflare

Protect each MCP path with its own Cloudflare Access application. Enable Managed
OAuth and allow only the intended account and the exact OpenAI redirect URI for
that connection. Cloudflare handles the sign-in and sends a signed assertion to
the hub. Use separate Access application audience tags for Dot and Codex Cloud.

Add `cloudflare-access.json` to the private PigeonMQ hub home. Set the file mode
to `0600`. Set only the bindings that you use.

```json
{
  "issuer": "https://example-team.cloudflareaccess.com",
  "email": "owner@example.com",
  "dot": {"audience": "<dot-app-audience>", "bearer_file": "dot-mailbox.key"},
  "codex": {"audience": "<codex-app-audience>", "bearer_file": "codex-mailbox.key"}
}
```

Use the Access team issuer shown in the Cloudflare settings. Each bearer file is
private and contains that mailbox token, with an optional `Bearer ` prefix. The
hub checks the signed assertion before applying the existing mailbox policy. Test each direct
connection and its Events subscription on the target OpenAI surface before
switching users. Cloudflare Access serves the discovery URLs from its challenge.

## Back up the operator registry before an upgrade

Stop the registration service. Back up its private home and the operator binary before
you open the registry with a newer build. The Unix origin preview adds a private
origin-binding table and upgrades the SQLite schema to version 3. Version 2 binaries
refuse to open an upgraded registry. Existing TCP registrations are not converted.

To roll back before the upgraded service accepts work, restore the matching backup and
binary together. If the upgraded service accepted work, do not replace its registry
with an older copy. Quiesce the service and reconcile the affected installations first.

## Manage a registration

Run these commands on the operator Mac with the same private home:

```sh
pigeonmq-registration --home /path/to/private-registration-home --revoke <installation-id>
pigeonmq-registration --home /path/to/private-registration-home --reissue <installation-id>
```

Revoke removes the installation's Cloudflare route and tunnel. The command reports completion only after a Cloudflare read shows removal. If the operation stays pending, retry with the same installation ID. Reissue prepares the same address for a new tunnel. The original installer uses its existing installation proof to finish setup.

The serving process checks for expired tunnels every 15 minutes. It starts removal at 12 hours after issue only if Cloudflare reports that the tunnel never connected. The service keeps installations with a recorded connection and skips unknown states. Cloudflare does not offer an atomic check for prior connection at deletion. The expiry path never forces a disconnect.

Check the Mac installer, live provider behavior, and each real cloud client before starting hosted service.
