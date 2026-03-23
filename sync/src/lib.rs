//! Sync protocol for the community interpretation index.
//!
//! Interpretations are broadcast-only, offline-first p2panda operations.
//! A peer who has been offline catches up by syncing with any online peer
//! who holds newer operations.
//!
//! The `p2panda-sync` dependency will be added when crate versions are pinned.
