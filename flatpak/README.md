# Flatpak packaging

## One-time setup

Install the required tools and runtimes:

```sh
flatpak install flathub org.gnome.Platform//47 org.gnome.Sdk//47
flatpak install flathub org.freedesktop.Sdk.Extension.rust-stable//23.08
```

Get the cargo sources generator:

```sh
curl -O https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
pip3 install aiohttp toml
```

## Generate cargo-sources.json

Run this from the `flatpak/` directory whenever `Cargo.lock` changes:

```sh
python3 flatpak-cargo-generator.py ../Cargo.lock -o cargo-sources.json
```

This file is large (~hundreds of entries) and must be committed alongside the manifest.

## Local test build

```sh
flatpak-builder --user --install --force-clean build-dir io.github.bendik.Zodia.yml
flatpak run io.github.bendik.Zodia
```

## Flathub submission

1. Tag the release: `git tag v0.4.0 && git push --tags`
2. Fill in the commit SHA in the manifest (`commit:` field)
3. Fork https://github.com/flathub/flathub
4. Create branch `new-pr/io.github.bendik.Zodia`
5. Copy `io.github.bendik.Zodia.yml` and `cargo-sources.json` into the branch root
6. Open a pull request — CI will validate the build automatically
