# Hosted registration operator preview

The registration service manages the address and Cloudflare Tunnel for each invited installation. It listens on loopback. User Macs serve their own MCP endpoints and store their own queues.

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
  "invited_accounts": {"pilot@example.com": "11111111-1111-4111-8111-111111111111"},
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

Open the private installation link from PigeonMQ. Enter the invited email address and the numeric email code. Do not enter a code from the Mac.
The link expires with the reservation. Open it only in the intended browser. Do not share or log the full link.
Opening the link does not send an email. The browser removes the private fragment and submits it only when you request the email code.
Supabase issues the numeric OTP for an email address, not an installation.
PigeonMQ applies approval only to the installation bound to the browser attempt.
The request page does not show whether it sent an email. Response times can differ.
If an attempt fails, wait up to five minutes, then reopen registration from PigeonMQ. Use the same pending installation until its reservation expires.

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

## Manage a registration

Run these commands on the operator Mac with the same private home:

```sh
pigeonmq-registration --home /path/to/private-registration-home --revoke <installation-id>
pigeonmq-registration --home /path/to/private-registration-home --reissue <installation-id>
```

Revoke removes the installation's Cloudflare route and tunnel. The command reports completion only after a Cloudflare read shows removal. If the operation stays pending, retry with the same installation ID. Reissue prepares the same address for a new tunnel. The original installer uses its existing installation proof to finish setup.

The serving process checks for expired tunnels every 15 minutes. It starts removal at 12 hours after issue only if Cloudflare reports that the tunnel never connected. The service keeps installations with a recorded connection and skips unknown states. Cloudflare does not offer an atomic check for prior connection at deletion. The expiry path never forces a disconnect.

Check the Mac installer, live provider behavior, and each real cloud client before starting hosted service.
