# Flatpak packaging

The Flathub submission lives at `flatpak/flathub/` — a git submodule tracking
[flathub/io.github.bendik.Zodia](https://github.com/flathub/io.github.bendik.Zodia).

## One-time setup

### Runtimes
```sh
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50
flatpak install flathub org.freedesktop.Sdk.Extension.rust-stable//24.08
```

### Python deps (for cargo-sources regeneration)
```sh
pip install aiohttp toml
```

### cargo-release (for the release chore)
```sh
cargo install cargo-release
```

## Making a release

1. Write the release description in `app/data/io.github.bendik.Zodia.metainfo.xml`
   (add a `<release>` entry under `<releases>` — the hook will insert a blank one if missing).
2. Run:
   ```sh
   cargo release <version>
   ```
   This will:
   - Bump `[workspace.package] version` in `Cargo.toml`
   - Stage the updated metainfo.xml
   - Commit, tag (`v<version>`), and push to origin
   - Update the flathub submodule (manifest tag/commit, regenerate `cargo-sources.json`,
     sync metainfo), commit + push to Flathub, then record the submodule bump in zodia

3. Dry-run first if unsure:
   ```sh
   cargo release --dry-run <version>
   ```

## Local test build

From the repo root:
```sh
flatpak-builder --user --install --force-clean build-dir flatpak/flathub/io.github.bendik.Zodia.yml
flatpak run io.github.bendik.Zodia
```

## Manual cargo-sources regeneration

If you need to regenerate `cargo-sources.json` outside of a release (e.g. after pulling
a dependency change):
```sh
python3 flatpak/flatpak-cargo-generator.py Cargo.lock \
  -o flatpak/flathub/cargo-sources.json
```
Then commit the result inside the submodule and push.
