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

### CI credential (one-time, maintainer's own GitHub account)

`.github/workflows/flathub-sync.yml` needs write access to the separate
`flathub/io.github.bendik.Zodia` repo, which the default `GITHUB_TOKEN`
doesn't have (it's scoped to this repo only).

**Not a deploy key**: adding one requires *admin* on that repo, and a
maintainer is normally only a push-access collaborator there (Flathub's
org keeps admin for itself) — confirmed via
`gh api repos/flathub/io.github.bendik.Zodia --jq .permissions` returning
`admin: false, push: true`. A fine-grained PAT hits the same wall: repos
outside your own account need the resource-owning org to pre-approve
fine-grained token access, and Flathub hasn't opted into that.

What actually works: a **classic PAT** (`repo` scope) on the maintainer's
own account — it simply inherits whatever access the account already has,
with no per-repo approval step.

1. Create one at github.com/settings/tokens/new (classic, `repo` scope).
2. Store it, without ever pasting the raw value into a chat/log:
   `gh secret set FLATHUB_PAT --repo bendik/zodia` (prompts on stdin).

Without this, `flathub-sync.yml` runs but fails at the push/PR step —
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
