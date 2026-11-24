# Local Codex catalog preview

Use a clean macOS VM with a GUI login. Do not use this catalog in your regular account.
This preview does not publish a plugin or enable remote access.

## Build the catalog

On a Mac with the same architecture as the guest, build the native release archive.

```sh
scripts/package-release.sh aarch64-apple-darwin
archive=target/artifacts/999k-0.2.0-aarch64-apple-darwin.tar.gz
sha=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
python3 packaging/codex/render.py "$archive" "$sha" target/codex-test-catalog
```

Use the archive from this build. Do not use the older published archive.
On Intel, replace `aarch64` with `x86_64`. Use a new catalog destination.
The renderer copies the native binary and license. It does not download anything.

## Install and test

In the disposable guest account, copy the rendered catalog to a local folder.
Set `CODEX_HOME` to a new directory for this test. Use an installed Codex CLI.
Create the explicit preview marker. Do not create the hub directory yourself.

```sh
umask 077
mkdir "$HOME/.pmq-codex-preview"
printf '%s\n' pigeonmq-codex-local-preview-v1 > "$HOME/.pmq-codex-preview/enable"
export CODEX_HOME="$HOME/pmq-test-codex"
codex plugin marketplace add /absolute/path/to/codex-test-catalog
codex plugin add radio999k-preview@radio999k-local-test
```

Start a fresh local Codex session. Check that 999K appears in the MCP list.
On first use, the plugin copies its binary to the private preview directory.
It then starts the managed hub and connects to one local Cue mailbox.
It uses the existing mailbox policy and private Unix socket.
The plugin does not use an email code, Keychain, or Cloudflare.
It does not reuse a different existing hub. Concurrent first starts wait briefly for the same hub.

The preview binds the normal hub port on loopback. Do not run it alongside another hub.
Stop if a different 999K job or binary exists. No upgrade workflow is provided.

## Remove the test

Remove the plugin only from the disposable Codex home.
Closing a chat or removing the plugin does not stop the hub or delete the queue.
The binary, queue, and bearer are outside the plugin cache.

To stop the hub before you discard the guest, retire its Cue profile with the installed preview binary:

```sh
"$HOME/.pmq-codex-preview/pigeonmq" remove cue --apply --home "$HOME/.pmq-codex-preview/hub"
```

Retirement does not delete the queue. Discard the guest to erase its synthetic data.
