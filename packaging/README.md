# Release installers

Build and smoke-test each native archive before rendering its installers.

```sh
python3 packaging/render.py 0.1.6 target/artifacts target/distribution
```

The archive directory must contain `SHA256SUMS` and the archives it lists.
Use a new output directory. The renderer checks the archive hashes and layout.
It generates three install paths for the same release:

- `install.sh`: a release asset. It installs both binaries in `$HOME/.local/bin` by default.
  Use `--bin-dir /absolute/path` to choose another writable directory.
  It does not replace existing files that differ from the release binaries.
- `npm/`: the unpublished npm package. It contains the exact generated script.
  The launcher verifies both files before each run. It keeps them in
  `$HOME/.local/share/pigeonmq/npm/<version>-<target>/bin`, not in the npm cache.
- `pigeonmq.rb`: the unpublished Homebrew formula. Homebrew checks the pinned
  archive SHA-256 before installation.

The direct and npm installers require `sh`, `curl`, `tar`, and either `sha256sum`
or `shasum`. None of the installers starts a service, configures a tunnel, or
copies credentials. Only the renderer requires Python.

Rendering does not publish anything. Publish the archives before enabling the
npm package or formula so their pinned URLs can resolve.
