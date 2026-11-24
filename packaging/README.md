# Release installers

Build and smoke-test each native archive before rendering its installers.

```sh
python3 packaging/render.py 0.2.0 target/artifacts target/distribution
```

The archive directory must contain `SHA256SUMS` and the archives it lists.
Use a new output directory. The renderer checks the archive hashes and layout.
It generates three install paths for the same release:

- `install.sh`: a release asset. It installs both binaries in `$HOME/.local/bin` by default.
  Use `--bin-dir /absolute/path` to choose another writable directory.
  It does not replace existing files that differ from the release binaries.
  If another install holds the directory lock, try again after it stops.
  After a forced stop, verify no installer is running before removing a stale
  `.pigeonmq-install.lock` directory.
- `npm/`: the unpublished npm package. It contains the exact generated script.
  The launcher verifies both files before each run. It keeps them in
  `$HOME/.local/share/pigeonmq/npm/<version>-<target>/bin`, not in the npm cache.
- `pigeonmq.rb`: the unpublished Homebrew formula. Homebrew checks the pinned
  archive SHA-256 before installation.

The direct and npm installers require `sh`, `curl`, `tar`, and either `sha256sum`
or `shasum`. None of the installers starts a service, configures a tunnel, or
copies credentials. The installer renderer requires Python.

Rendering does not publish anything. Publish the archives before enabling the
npm package or formula so their pinned URLs can resolve.

## Orca plugin archive

The release workflow builds a separate Orca plugin archive once, with its own
checksum. Build it locally with Python 3:

```sh
python3 packaging/package_orca.py 0.2.0 target/orca-plugin
python3 tests/orca_release_package.py
(cd target/orca-plugin && shasum -a 256 orca-pigeonmq-0.2.0.tar.gz > SHA256SUMS)
```

The version must match the plugin manifest. The archive contains a single
installable folder, `orca-pigeonmq-0.2.0`. Keep the Orca checksum with the Orca
archive in its own directory. The native installer checksum file lists platform
archives only.

Install PigeonMQ 0.2.0 with the native installer. The plugin's setup button uses
the default executable at `$HOME/.local/bin/pigeonmq`. Use Orca 1.4.268 or newer
and run these commands from the directory containing the Orca archive and checksum.

```sh
shasum -a 256 --check SHA256SUMS
tar -xzf orca-pigeonmq-0.2.0.tar.gz
orca plugin validate ./orca-pigeonmq-0.2.0
orca plugin install ./orca-pigeonmq-0.2.0
```

Review Orca's requested workspace and terminal permissions and approve the
specific archive contents if they match your intent. For a custom PigeonMQ
executable path, run that executable's `init --orca` in a local Orca terminal.
