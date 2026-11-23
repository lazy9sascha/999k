# Hosted Cue Bearer preview

The finite Bearer mode is opt-in. It applies only to the managed hosted Unix
origin at `/cue/mcp`. It does not apply to local stdio, loopback, or another mailbox.
Other hosted profiles still use their existing policy. Do not treat this preview
as the full hosted credential lifecycle.

Before the first rotation, stop the owned managed hub. This prevents an older
process from continuing to accept the local Bearer on the hosted route.
Choose a lifetime in seconds. Use a new file in an absolute owner-private
directory outside the hub home:

```sh
pigeonmq hosted-cue-bearer --home /private/hub rotate \
  --lifetime-secs <seconds> --export-file /private/transfer/cue-bearer
```

Choose 1 to 2,592,000 seconds. The preview has no default lifetime.
The command stores the raw Bearer in the new mode 600 file only.
Transfer it to the authorized client through your protected credential channel.
Do not paste it into a command, log, or chat. The tunnel connector token is separate.

The first rotation enables finite mode for this installation. The previous local
Bearer then fails on the hosted route. It continues to work on local stdio.
Start the same managed hub with the new binary after the first rotation.
A second rotation replaces the remote Bearer. The previous remote Bearer fails
without restarting the hub. If a request is holding the grant, wait for that
request to finish before you retry the rotation.

To revoke without issuing a replacement:

```sh
pigeonmq hosted-cue-bearer --home /private/hub revoke
```

A revoked, expired, or missing finite grant denies hosted Cue access.
Revocation does not remove the mailbox, modify its policy, or delete queue data.
Keep the managed lifecycle registry with the installation. It records that
finite mode is enabled. Do not start an older binary with this registry.
For rollback, first leave hosted access disabled. Do not restore a snapshot that
re-enables the old remote Bearer.
