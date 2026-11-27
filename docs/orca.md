# Connect your agents to Orca

Connect Dot (ChatGPT), Muse, GrokBot, Cue, or Instinct (Messages). Ask an
agent from your Orca project chat and check for its linked reply there.

P-999K is experimental. You can use a hosted address or your own HTTPS
domain. Both use the queue on your Mac.

## Install the CLI and plugin

Use an Apple Silicon Mac with Orca 1.4.268 or later. If the Mac already
runs a managed hub, follow the [upgrade steps](../packaging/README.md#upgrade-an-existing-installation).
For a new installation, run the 999K CLI installer in an empty directory.

```sh
(
	set -e
	curl -q --proto '=https' --proto-redir '=https' -fLsS -o install.sh \
		https://github.com/lazy9sascha/p-999k/releases/download/0.2.2/install.sh
	sh ./install.sh
	"$HOME/.local/bin/999k" --version
)
```

Check that the CLI prints `999k 0.2.2`. In Orca, open **Settings → Plugins**.
Turn on plugins if prompted. Search for **P-999K** by **lazy9sascha** in
the Community catalog. Select **Review & install** or **Install**.
Check the requested permissions, then select **Install & enable**.
If Orca asks for confirmation, select **Install**.
The plugin uses them to find your workspace and run setup in a terminal.
[Install from a release archive](../packaging/README.md#orca-plugin-archive).

## Connect a peer

1. Open a local workspace. In the right sidebar, choose
   **P-999K → Initial setup → Open setup terminal** to prepare the local hub.
2. Choose how the app will reach your Mac.

   - For Muse, GrokBot, Cue, or Instinct,
     [reserve a hosted address](#reserve-a-hosted-address) or
     [configure the peer's routes on your domain](#use-your-own-https-domain).
   - For Dot, use a protected OAuth endpoint your operator prepared or a
     [private Secure MCP Tunnel](configuration.md#dot-through-a-private-secure-mcp-tunnel).
     If you manage the domain, [configure Dot's OAuth endpoint](configuration.md#dot-and-codex-cloud-through-direct-oauth).

3. In **Setup target**, select your peer. Choose **I’ll do it myself → Open setup terminal**.
   If Orca asks you to create or approve the relay, follow the prompt.
   Run any resume command in the same workspace.
4. Finish setup in the app you already use.

   - For pairing, wait for **authenticated MCP route ready**. Copy the setup
     message from the panel or terminal into the peer's existing conversation.
     When the peer opens its private setup, choose **Get a setup code** in
     Orca. Enter the address and code from the terminal in the app's
     private fields. If the app asks for a Bearer token,
     [redeem the code privately](tunnels.md#offer-and-redeem)
     and use the app's secure credential field.
   - For direct Dot, wait for **OAuth discovery ready**. Add the protected
     URL as a private MCP app in ChatGPT. Before you approve OAuth, read the
     notice in the setup terminal or [check what OAuth uses and what the hub
     stores](configuration.md#oauth-sign-in-and-message-data).
     If you use a private tunnel, select its app in the intended Dot conversation.

5. Ask the app to list its mailbox tools. Then
   [verify the first reply](#verify-the-first-reply).

To delegate setup, select **Ask my assistant to do it** for a single peer,
or **Ask my assistant to set up all peers** under **Initial setup**. Copy the
request to an assistant with Computer Use. Follow any browser or app
approval it asks you to complete.

### Reserve a hosted address

P-999K resets its hosted tunnel addresses every 5 days. Peers using those
addresses lose access at each reset. Your queue stays on your Mac.
Contact your operator to restore a hosted address.
For a stable address, [use your own HTTPS domain](#use-your-own-https-domain).

With direct email delivery, your operator's email provider receives the
address and one-time code to send the message. The registrar keeps temporary
verification digests. After approval, it keeps the verification result,
installation status, and tunnel references. If enabled, the operator email
contains only the time when the address was first issued. The email provider
applies its own mail retention policy.

If the registration page names Supabase Auth, that service stores the email
with its account record. [Supabase Auth user records](https://supabase.com/docs/guides/auth/users)

Cloudflare serves HTTPS for the routes you attach. The hub keeps your queue
and mailbox credentials on your Mac.

Install [cloudflared for macOS](https://developers.cloudflare.com/tunnel/downloads/#macos).
Choose **Initial setup → Reserve hosted address**. In the browser, enter the
code from your verification email. Return to Orca and continue with the peer
you selected. The other eligible peers can use the same address.

If registration stops waiting, select **Reserve hosted address** again.
From an owner terminal, you can reserve with the same hub home as Orca.

```sh
999k init --reserve-address --home "$HOME/path/to/private-hub"
```

When the command reports `hosted_connector=staged`, return to Orca and
select a peer. [Check a hosted address](troubleshooting.md#check-a-hosted-address)

## Verify the first reply

1. In your Orca project chat, ask the project agent to consult a peer you
   connected. For example, ask "Have Dot review this plan for missing
   steps." **Queued** means the hub stored the request.
2. If the peer waits for a manual check, open its existing conversation
   and ask it to check its P-999K mailbox.
3. Look for the answer in your original Orca project chat. If the relay
   finishes waiting before the answer arrives, ask it to check the same
   request. [Troubleshoot a missing reply](troubleshooting.md#check-a-pending-orca-reply)

[Check which peers verified automatic intake](research.md#new-orca-installation-with-a-hosted-address)

<a id="switch-to-your-own-domain"></a>

## Use your own HTTPS domain

In Orca, choose **Initial setup → Use my own domain**. Use the same hub
home to keep your queue and mailbox credentials.

1. Point your [HTTPS proxy or tunnel](tunnels.md#use-your-own-https-origin)
   at the hub. Publish the peer's exact
   [MCP and pairing paths](tunnels.md#pairing-routes). For Dot, publish its
   [protected OAuth endpoint](configuration.md#dot-and-codex-cloud-through-direct-oauth).
2. Add the peer ID and exact HTTPS MCP URL to the owner-private
   `orca-ingress.json` in that home. For example, use
   `{"muse":"https://radio.example.com/muse/mcp"}`. Keep the other entries and
   set the file mode to `0600`. This entry takes priority over the saved
   hosted address for that peer.
3. If this is your first connection, return to [Connect a peer](#connect-a-peer)
   and select the peer in **Setup target**. For an existing peer, open its
   setup terminal again and update the endpoint in the app.
4. [Verify a reply](#verify-the-first-reply) before retiring an existing
   connection. To roll back, restore the previous endpoint in the app
   and restore or remove that peer’s entry in `orca-ingress.json`.

## Set up a management bot

1. In the panel, choose **Initial setup → Set up a management bot**.
2. In **Bots → New bot**, create **P-999K**. Use the purpose shown in the panel.
3. In the bot's **Home Chat**, ask it to connect a peer by name. To switch
   domains, ask “Use my own domain.” If an older bot lacks that guide,
   update its purpose from the panel.

[Orca CLI commands and options](configuration.md#orca-setup-commands)
