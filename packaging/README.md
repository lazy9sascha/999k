# Release installers

Build and smoke-test each native archive before rendering its installers.

```sh
python3 packaging/render.py 0.2.1 target/artifacts target/distribution
```

The archive directory must contain `SHA256SUMS` and the archives it lists.
Use a new output directory. The renderer checks the archive hashes and layout.
It generates three install paths for the same release:

- `install.sh`: a release asset. It installs both binaries in `$HOME/.local/bin` by default.
  When the matching native archive is beside the downloaded script, the installer uses that file and verifies the same pinned checksum.
  Use `--bin-dir /absolute/path` to choose another writable directory.
  It does not replace existing files that differ from the release binaries.
  If another install holds the directory lock, try again after it stops.
  After a forced stop, verify no installer is running before removing a stale
  `.pigeonmq-install.lock` directory.
- `npm/`: the unpublished npm package. It contains the exact generated script.
  The launcher verifies both files before each run. It keeps them in
  `$HOME/.local/share/999k/npm/<version>-<target>/bin`, not in the npm cache.
- `radio999k.rb`: the unpublished Homebrew formula. Homebrew checks the pinned
  archive SHA-256 before installation.

The direct and npm installers require `sh`, `curl`, `tar`, and either `sha256sum`
or `shasum`. None of the installers starts a service, configures a tunnel, or
copies credentials. The installer renderer requires Python.

Rendering does not publish anything. Publish the archives before enabling the
npm package or formula so their pinned URLs can resolve.

The release needs three native archives: macOS on Apple Silicon, Linux on x86-64, and Linux on ARM64. Each archive contains both executables.

## Upgrade an existing installation

The new installer writes `999k` and `999k-dot-resolve`. It leaves older
`pigeonmq` executables, your hub home, queue, bearer files, and service settings
in place.

A managed hub's LaunchAgent and an Orca tool may point at an absolute executable
path. To upgrade those callers without rebinding them, back up the installed
binary and place the verified new binary at the same path as a regular executable.
Keep the existing owner and permissions. A symlink to a different executable
may fail managed service ownership checks. Use your existing service's restart
procedure and verify the original home with `999k health --home <existing-home>`.

Use `RADIO999K_BIN` to refer to the new executable when following the quickstart.
The Homebrew formula is named `radio999k` and installs commands named `999k`
and `999k-dot-resolve`. The npm package is named `@lazy9sascha/999k`.

## Orca plugin archive

The release workflow builds a separate Orca plugin archive once, with its own
checksum. Build it locally with Python 3:

```sh
python3 packaging/package_orca.py 0.2.1 target/orca-plugin
python3 tests/orca_release_package.py
(cd target/orca-plugin && shasum -a 256 p-999k-0.2.1.tar.gz > SHA256SUMS-orca)
```

The plugin version comes from its manifest and can differ from the CLI version. The archive contains a single
installable folder, `p-999k-0.2.1`. Keep the Orca checksum with the Orca
archive in its own directory. The native installer checksum file lists platform
archives only.

Install the [0.2.1 CLI](../docs/quickstart.md#connect-orca-to-your-existing-peers) first. The plugin runs `$HOME/.local/bin/999k`. The 0.2.0 release archive lacks the required `--orca-peer` and `pair` commands. Use Orca 1.4.268 or newer
and run these commands from the directory containing the Orca archive and its release checksum file.

```sh
shasum -a 256 --check SHA256SUMS-orca
tar -xzf p-999k-0.2.1.tar.gz
orca plugin validate ./p-999k-0.2.1
orca plugin install ./p-999k-0.2.1
```

The catalog ID is `lazy9sascha.p-999k`. Existing local installations of 0.2.0 use `local.radio999k`. The trusted worker uses `workspace:read` to find the active local workspace and `terminal:send` to run the setup command in a terminal there. To remove the installed plugin and its approval, run `orca plugin uninstall lazy9sascha.p-999k`. The first install can stop to request approval. Review the access and use the approval command Orca prints for that exact archive. After installation, open **P-999K** in your local workspace. For a custom 999K
executable path, run that executable's `init --orca` in a local Orca terminal.

For a catalog release, copy the extracted files and the plugin README to the root of the public plugin repository. Add the `orca-plugin` topic and tag the commit with the plugin version, such as `v0.2.1`.
