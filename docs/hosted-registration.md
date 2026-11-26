# Use a hosted address

Your Mac keeps the queue and mailbox credentials. Supabase verifies your
email for registration. Cloudflare serves HTTPS for the peer routes you
attach. [Use your own HTTPS domain](orca.md#switch-to-your-own-domain)

<a id="reserve-an-address-without-starting-a-connector"></a>

## Reserve your address

In Orca, choose **P-999K → Initial setup → Reserve hosted address**.
Enter the code from your verification email in the browser. Return to Orca
and select the peer you want to connect.
[Finish connecting the peer](orca.md#connect-a-peer)

To reserve from a terminal, use the same hub home as Orca.

```sh
999k init --reserve-address --home "$HOME/path/to/private-hub"
```

If the command stops waiting, run it again with the same home. When you
see `hosted_connector=staged`, select a peer in Orca to attach its route.
If your CLI has no bundled registration URL, use the HTTPS registration
origin your operator supplied. [Registration URL setting](configuration.md#orca-setup-commands)

<a id="what-users-see"></a>

## Check your address

Run `999k hosted-status --home DIR` on the hub Mac. An unused address can
expire if its tunnel never connects. If access expires or is revoked,
contact the operator who provided it. Your queue and mailbox credentials
stay on your Mac.

<a id="connect-dot-and-codex-cloud-through-cloudflare"></a>

[Configure a direct OAuth endpoint for Dot or Codex Cloud](configuration.md#dot-and-codex-cloud-through-direct-oauth)
