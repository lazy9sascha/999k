# Hosted registration operator preview

The registration service manages the address and Cloudflare Tunnel for each invited installation. It listens on loopback. User Macs serve their own MCP endpoints and store their own queues.

## Prepare a private pilot

1. Create an owner-only directory with mode 0700 on the operator Mac.
2. Save the settings below in `settings.json` with mode 0600. Replace example values with the approved pilot settings.
3. Save the Cloudflare management token in `cloudflare.key` with mode 0600. Grant tunnel management for the operator account and DNS access for the selected zone.
4. Enable the chosen OAuth provider in Supabase Auth. Register the exact `/auth/callback` URL under the registration origin. Add the test users' Supabase account IDs to `invited_accounts`.
5. Route the registration origin to the loopback service over HTTPS. Preserve the public registration Host header. Disable caching. Keep credentials and callback query strings out of logs.

```json
{
  "registration_origin": "https://init.example.com",
  "supabase_url": "https://project.example.com",
  "supabase_publishable_key": "sb_publishable_example",
  "login_provider": "github",
  "invited_accounts": ["11111111-1111-4111-8111-111111111111"],
  "cloudflare_account_id": "<account-id>",
  "cloudflare_zone_id": "<zone-id>",
  "cloudflare_zone_name": "example.com",
  "hub_port": 54843,
  "mcp_path": "/muse/mcp"
}
```

Use the loopback port and MCP path that the pilot installs on user Macs.

```sh
cargo build --locked --bin pigeonmq-registration
./target/debug/pigeonmq-registration --home /path/to/private-registration-home --port 54900
```

The service creates `enrollments.sqlite3` in the private directory. It reserves an address before sign-in. After browser approval, it publishes only the configured MCP path and returns the installation's connector token.

## Manage a registration

Run these commands on the operator Mac with the same private home:

```sh
pigeonmq-registration --home /path/to/private-registration-home --revoke <installation-id>
pigeonmq-registration --home /path/to/private-registration-home --reissue <installation-id>
```

Revoke removes the installation's Cloudflare route and tunnel. The command reports completion only after a Cloudflare read shows removal. If the operation stays pending, retry with the same installation ID. Reissue prepares the same address for a new tunnel. The original installer uses its existing installation proof to finish setup.

The serving process checks for expired tunnels every 15 minutes. It starts removal at 12 hours after issue only if Cloudflare reports that the tunnel never connected. The service keeps installations with a recorded connection and skips unknown states. Cloudflare does not offer an atomic check for prior connection at deletion. The expiry path never forces a disconnect.

Check the Mac installer, live provider behavior, and each real cloud client before starting hosted service.
