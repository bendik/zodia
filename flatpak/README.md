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
pip install aiohttp tomlkit
```

### cargo-release (for the release chore)
```sh
cargo install cargo-release
```

### CI deploy key (one-time, repo admin only)

`.github/workflows/flathub-sync.yml` needs write access to the separate
`flathub/io.github.bendik.Zodia` repo, which the default `GITHUB_TOKEN`
doesn't have (it's scoped to this repo only):

1. Generate a dedicated keypair: `ssh-keygen -t ed25519 -f flathub-deploy-key -N ""`
2. Add `flathub-deploy-key.pub` as a **Deploy key** (with write access) on
   `flathub/io.github.bendik.Zodia`'s repo settings.
3. Add the private key (`flathub-deploy-key`) as a **repository secret**
   named `FLATHUB_DEPLOY_KEY` on `bendik/zodia`.
4. Delete both local key files.

Without this, `flathub-sync.yml` runs but fails at the SSH push step —
`cargo release` itself and the GitHub release/binaries are unaffected either way.

## Making a release

1. Write the release description in `app/data/io.github.bendik.Zodia.metainfo.xml`
   (add a `<release>` entry under `<releases>` — the hook will insert a blank one if missing).
2. Run:
   ```sh
   cargo release <version>
   ```
   This bumps `[workspace.package] version`, stages metainfo.xml, commits, tags
   (`v<version>`), and pushes to origin. **cargo-release itself does not touch
   Flathub** — it only supports a `pre-release-hook`, not a post-release one (see
   `release.toml` / `scripts/release-prep.sh`). The Flathub sync is a separate step:

   - Automatically, via `.github/workflows/flathub-sync.yml`, triggered by the
     same tag push — commits + pushes a branch on Flathub's repo and opens a PR.
   - Or manually: `bash scripts/flatpak-sync.sh <version>` (no `v` prefix).

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
