# Hosted registration operator preview

On the user’s Mac, reserve an address before selecting a peer:

```sh
"$HOME/.local/bin/999k" init --reserve-address
```

The CLI opens the operator’s registration page. After email verification,
select a peer to attach its route to the saved address.
[Install the CLI, Orca plugin, and cloudflared](quickstart.md#connect-orca-to-your-existing-peers).

The registration service manages the address and Cloudflare Tunnel for each approved installation. It listens on loopback. User Macs serve their own MCP endpoints and store their own queues.

## Responsibilities and installer handoff

- The 999K registration service asks Cloudflare to provision one hostname, route, and tunnel per installation. It keeps the Cloudflare management credential on the operator Mac. Cloudflare serves public HTTPS and forwards only the configured routes. The operator uses the registration service to revoke one installation while the operator Mac is running.
- The 999K hub owns mailbox identities, mailbox credentials, message delivery, linked replies, and acknowledgments. The user Mac stores the queue and mailbox credentials. Supabase sign-in approves registration only. It does not authorize mailbox access.

The installer sends a request ID and user code with a private installation proof.
Begin returns an installation ID, assigned public URL, private browser link, and expiry.
Cloudflare provisioning starts after approval. Status returns the registration
state and public URL. Only finish returns the connector token.
After approval, finish returns the ready state, public URL, and connector token.
Keep the same request ID, proof, and code on a begin retry. Use the same
installation ID and proof to retry status or finish. Do not request a new address
after a temporary error.

Verify the assigned route and an authenticated MCP exchange after finish.

## Prepare a private pilot

1. Create an owner-only directory with mode 0700 on the operator Mac.
2. Save the settings below in `settings.json` with mode 0600. Replace example values with the approved pilot settings.
3. Save the Cloudflare management token in `cloudflare.key` with mode 0600. Grant tunnel management for the operator account and DNS access for the selected zone.
4. In a dedicated test Auth project, enable email sign-in and account creation. Include the numeric code (`{{ .Token }}`) in both the **Confirm signup** and **Magic Link** email templates. Test one new email address and one returning address through the installer. A link-only email cannot complete this flow. Do not enable signups on a shared or live project as part of this preview. Direct requests to the public Auth API can create accounts when signups are enabled. 999K's installation and attempt checks apply only to its own registration endpoints.
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
  "mcp_path": "/cue/mcp"
}
```

The public pilot supports Cue. Set `hub_port` to 54843, the managed user hub port, and publish only `/cue/mcp`. Unix connector setup also publishes its exact sibling `/cue/pair`. Use loopback port 54900 for the registration service on the operator Mac. The user hub accepts its assigned public Host on the managed Unix socket. It also accepts the loopback Host used by its local readiness probe.

For a separately configured operator, set
`"mcp_paths": ["/dot/mcp", "/codex/mcp"]` to publish both paths on the same
hostname. Use `mcp_path` for one path or include it alongside `mcp_paths` to
retain an existing path. Dot and Codex also get their exact path-scoped OAuth
resource metadata paths. Enable the matching profiles and auth on the hub.

```sh
cargo build --locked --bin 999k-registration
./target/debug/999k-registration --home /path/to/private-registration-home --port 54900
```

The service creates `enrollments.sqlite3` in the private directory. It reserves an address before sign-in. After browser approval, it publishes only the configured MCP and discovery paths and returns the
installation's connector token. A successful finish on an existing installation
also reconciles its ingress with the current settings.

Open the private installation link from 999K. Enter your email address and the numeric email code. You do not need an existing Supabase Auth account. Do not enter a code from the Mac.
Remove the obsolete `invited_accounts` field from older settings. If the field remains, the service warns on startup and does not use it to restrict registration.
The link expires with the reservation. Open it only in the intended browser. Do not share or log the full link.
Opening the link does not send an email. The browser removes the private fragment and submits it only when you request the email code.
Supabase issues the numeric OTP for an email address, not an installation.
999K checks that the verified account holds the exact email address for this attempt. It applies approval only to the installation bound to that attempt. The email code and the tunnel connector token do not grant MCP access. Existing MCP authentication does not change.
The request page does not show whether it sent an email. Response times can differ.
If an attempt fails, wait up to five minutes, then reopen registration from 999K. Use the same pending installation until its reservation expires.

## Reserve an address without starting a connector

For new reservations, set `selectable_mcp_paths` to the routes owners can
attach. Set it to `[]` to issue addresses with no attachable routes. If omitted,
it defaults to `mcp_paths` and `mcp_path`. Legacy registrations continue to
use their original settings. The plain reservation command binds the installation to a
private Unix socket and configures only a 404 default rule. The address is
reserved while the connector is staged.

An operator-configured CLI includes the HTTPS registration origin. For a source build without an operator, set `RADIO999K_REGISTRATION_URL` to your operator’s HTTPS origin. This setting also overrides a bundled origin. Keep the same private hub home on each run.

```sh
999k init --reserve-address --home "$HOME/path/to/private-hub"
```

Approve the installation in your browser. If setup stops waiting or you close the terminal,
run the same command to resume. Do not copy the browser link or the installation files to a
different machine.

999K reports `hosted_connector=staged` and `hosted_public_route=unverified`.
It saves the address and private connector credential in the owner-private hub home.
It does not create a mailbox, change a local hub, or start a connector.

In the Orca panel, choose **Initial setup**, then **Reserve hosted address**.
Use the same home and select a peer with `999k init --orca --orca-peer <id>`.
999K uses the saved registration origin and adds the selected path if the
operator allows it. The address, installation, and previously attached paths
are preserved. The connector starts after the matching managed profiles answer
on the private socket. `hosted_connector=scheduled` reports local setup.

For older pilot registrations, keep the original flags and setup procedure.
A receipt bound to TCP cannot be promoted to a Unix installation. Retiring a
managed profile that supplies a required path removes the hosted job and keeps
the registration credential.

### Connect Dot and Codex Cloud through Cloudflare

Protect each MCP path with its own Cloudflare Access application. Enable Managed
OAuth and allow only the intended account and the exact OpenAI redirect URI for
that connection. Cloudflare handles the sign-in and sends a signed assertion to
the hub. Use separate Access application audience tags for Dot and Codex Cloud.

Add `cloudflare-access.json` to the private 999K hub home. Set the file mode
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

For Orca Dot setup, add the already protected HTTPS `/dot/mcp` URL under
`dot` in the owner-private `orca-ingress.json`. When attached to a managed
hosted address, use that exact origin and restart the managed hub after adding
the binding. The hosted socket requires a valid Cloudflare Access assertion for
this Dot route. Run
`999k init --orca --orca-peer dot` to bind the relay and check discovery.
The check requires an anonymous 401 with the exact resource metadata URL,
and metadata for the configured resource and Access issuer. Then approve
the connection in ChatGPT. Verify an authenticated tool call and an original
ACK with its linked reply in the originating Orca chat.


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
999k-registration --home /path/to/private-registration-home --list
999k-registration --home /path/to/private-registration-home --status <installation-id>
999k-registration --home /path/to/private-registration-home --status <installation-id> --check-cloudflare
999k-registration --home /path/to/private-registration-home --revoke <installation-id>
999k-registration --home /path/to/private-registration-home --reissue <installation-id>
```

List and status read the private registry. The Cloudflare check reads the recorded
tunnel, DNS record, and any tracked Dot Access app. Provider results describe resources.
Use a linked reply in the original chat to verify the peer connection.

After you prepare the dedicated Dot Access app, track it against its ready
address-first installation:

```sh
999k-registration --home /path/to/private-registration-home \
  --track-dot-access <installation-id> --access-app <access-app-id>
```

Track an app dedicated to this installation's exact `/dot/mcp` path with
Managed OAuth enabled. Later route changes preserve that protected Dot path.
The hub still needs the private Access binding described above. The command
uses the operator's Cloudflare credential to verify the app and the saved
tunnel and DNS record. Grant that credential permission to read and remove the
tracked Access app. [Cloudflare Access application API](https://developers.cloudflare.com/api/resources/zero_trust/subresources/access/subresources/applications/)

Revoke removes the installation's public route and tunnel, then removes its
tracked Dot Access app. The command reports completion only after Cloudflare
confirms removal. If the operation stays pending, retry with the same
installation ID. Reissue prepares the same address for a new tunnel. The
original installer uses its existing installation proof to finish setup.
Prepare and track a new Dot Access app if the user wants Dot on the restored address.

The serving process checks for expired tunnels every 15 minutes. It starts removal at 12 hours after issue only if Cloudflare reports that the tunnel never connected. The service keeps installations with a recorded connection and skips unknown states. Cloudflare does not offer an atomic check for prior connection at deletion. The expiry path never forces a disconnect.

### What users see

Before users request an email code, the registration page explains the role of
Supabase Auth and Cloudflare, what the operator stores, and when an unused
hosted address may expire. The approval page directs users back to the Mac to
finish setup and connect their peers.

On the hub Mac, run `999k hosted-status` with the same `--home` used for setup.
The command reads the registration with the saved installation proof and
explains pending approval, expiry, and revocation. Contact the operator to
restore revoked public access. Revocation leaves the user's local mailbox in place.

Before enabling automated provisioning, use a manually provisioned pilot
installation with a real off-host Dot client. Use its own public HTTPS URL and
mailbox authentication. Verify that the sender receives a reply linked to the
original message and that 999K acknowledges the original. With the operator
Mac running, revoke that installation. Verify that its managed URL no longer
reaches its mailbox and that a separate installation still works.
