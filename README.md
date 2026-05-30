# Zodia

A peer-to-peer astrological companion. Zodia connects you with others who share meaningful chart alignments — no accounts, no servers — just enabling technology for the astrology-minded.

![Zodia screenshot](screenshot.png)

---

## How it works

Zodia announces a coarse astrological fingerprint (solar month + approximate location) to a peer-to-peer network. Other Zodia users appear in the Network view as their announces reach you — over your local network or across the wider [p2panda](https://p2panda.org) overlay.

When you and another user mutually consent, you exchange exact birth data over a direct encrypted channel, compute your synastry, and can chat, place voice calls, and share interpretations of specific aspects.

Interpretations are signed by their author and replicated across the network through the same gossip layer, so the body of community readings grows whenever Zodia users meet. There's no central authority — contributions stand on their own.

## Privacy

Everything you don't share stays on your device. Zodia's announces carry only your solar month and a rough location (~600 km); exact birth data is exchanged only after you choose to connect with someone. Interpretations you publish are signed with your local identity key and replicated across the network — they cannot be unpublished once shared. Zodia has no central authority to verify identities, so share at your own discretion.

## Building from source

**Linux (Ubuntu 24.04 or equivalent)**
```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libopus-dev libasound2-dev pkg-config
cargo build --release --bin zodia
```

**macOS**
```sh
brew install gtk4 libadwaita pango opus pkg-config glib
cargo build --release --bin zodia
```

Requires Rust 1.75+ and GTK 4 / libadwaita 1.4+.

## License

Zodia is free software: you can redistribute it and/or modify it under the terms of the [GNU Affero General Public License](LICENSE) as published by the Free Software Foundation, either version 3, or (at your option) any later version.

Because Zodia is peer-to-peer software, the AGPL's network-use clause applies: if you modify Zodia and let others connect to your modified version, you must make the source available to them under the same terms.
