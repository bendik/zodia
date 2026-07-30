# PRD: p2panda 0.6 → 0.7 migration (Phase D prerequisite)

**Status:** shipped
**Branch:** `main`
**Triggered by:** starting Phase D (`docs/prd/operations-and-streams-rearchitecture.md`) on `zodia-circles`. p2panda's group-encryption crate, `p2panda-encryption`, only exists at 0.7.0 and hard-requires `p2panda-core ^0.7.0`. The rest of our p2panda stack (`core`/`net`/`store`/`stream`/`sync`/`discovery`) was pinned at 0.6.1, so real circles work needed this upgrade first.

## Problem Statement

The Phase D PRD's own text ("wraps p2panda 0.6's group-encryption primitives") turned out to be wrong — no such crate exists at 0.6.x. Getting real group encryption (post-compromise security, forward secrecy) meant either bumping the whole p2panda stack to 0.7.0, or hand-rolling group crypto on top of `zodia-crypto`'s existing ECIES with none of those properties. Chose the upgrade.

## What changed upstream

Surveyed every p2panda symbol our code actually imports (not the full API surface) before touching `Cargo.toml`, by diffing vendored 0.6.1 registry sources against a scratch `cargo fetch` of 0.7.0. Most of the surface — `p2panda_core::{Hash, Operation, Body, SigningKey, VerifyingKey, Timestamp, Topic}`, `p2panda_net::{AddressBook, Discovery, Endpoint, Gossip, NodeId}`, `p2panda_store::{OperationStore, LogStore, TopicStore, SqliteStore}` trait shapes, `p2panda_stream::Processor` — is byte-identical, only line numbers shifted. The breaking changes that actually hit us:

- **Toolchain floor**: p2panda 0.7.0 requires rustc 1.96; sandbox had 1.95.0. Fixed with `rustup update stable` (now 1.97.1). No `rust-toolchain.toml` in this repo and CI's `dtolnay/rust-toolchain@stable` always tracks latest stable, so nothing else needed touching.
- **iroh 0.98 → 1.0.3**: a major-version jump, but the two iroh items we touch directly (`endpoint::{Connection, RecvStream, SendStream}`, `protocol::{ProtocolHandler, AcceptError}`) are unchanged — everything else goes through p2panda-net's own `Endpoint` wrapper, not raw iroh. Net risk from this jump: negligible in practice.
- **`Header::timestamp` removed.** p2panda 0.7 dropped the built-in timestamp field from `Header<E>` entirely — extensions (`Header<E>`'s `E` type, previously `()` everywhere in our code) are now the sanctioned place for per-operation metadata, per `p2panda-core`'s own `extensions.rs` example. Fixed by adding `zodia_ops::OpExtensions { timestamp: Timestamp }` implementing `Extension<Timestamp>`, and switching every `Operation<()>`/`Header<()>` in `zodia-sync` and `zodia-pipeline` to `Operation<OpExtensions>`/`Header<OpExtensions>`. Reads now go through `header.extension::<Timestamp>()` instead of a direct field.
- **`SeqNum` narrowed `u64` → `u32`.**  Mechanical fallout at a couple of call sites.
- **`OperationStore<T, ID, C>` → `OperationStore<T, ID>`.** The log-id type moved from a trait-level generic to a per-method generic on `insert_operation` alone. Turbofish call sites (`OperationStore::<Operation<_>, Hash, u64>::get_operation(...)`) needed their third argument dropped.
- **`SyncHandle::publish` de-asynced.** Was `async fn publish(...) -> Result<...>` in 0.6, is a plain sync `fn` returning `Result` directly in 0.7. Dropped the stray `.await`.
- **`operations_v1.timestamp` column removed** from `p2panda-store`'s own SQLite schema (consistent with `Header::timestamp`'s removal — there's nothing left to put in that column). This broke `zodia-sync::prune_older_than` at runtime (`no such column: timestamp`), caught by the existing `pruning.feature` cucumber scenarios, not by compilation — a good example of why the real-network BDD layer earns its keep. Fixed by reading `operations_v1.header` (still a CBOR blob, still written via `p2panda_core::cbor::encode_cbor` — same codec p2panda-store itself uses) back per-candidate-row, decoding each `Header<OpExtensions>` and filtering by its `Timestamp` extension in Rust, before doing the same bulk `DELETE ... WHERE hash IN (...)` this function always did. Still one query round-trip for the delete; the age check just moved from SQL to Rust since the column that made it a SQL-only filter no longer exists.
- **Bonus finding**: `p2panda-stream` 0.7.0 ships a new `groups` processor module (alongside `p2panda-auth`/`p2panda-spaces`/`p2panda-encryption`) — a ready-made `Processor<T>` implementation for group operations, not just a raw crypto crate to wire up ourselves. Directly relevant to the upcoming `zodia-circles` PRD: compose with it rather than write a bespoke access-control stage.

## Testing Decisions

No new tests written — this is a dependency migration, not a feature. Verification was the existing suite: `cargo test --workspace` (all unit tests, same pass counts as pre-migration) and the full `zodia-sdk` cucumber suite (13 features, 16 scenarios, 108 steps, all passing) run against the upgraded stack, including both `pruning.feature` scenarios that had caught the schema-change regression above.

## Out of Scope

- `zodia-circles` itself — this PRD is purely the prerequisite. Circles work continues next, and should lean on `p2panda-auth`/`p2panda-encryption`/`p2panda-spaces`/the new `groups` processor directly per the user's steer to prefer p2panda-native primitives over custom abstractions.
- No wire-format change for existing ops beyond the `OpExtensions` timestamp relocation — `InterpOp`/`DocOp` CBOR bodies are untouched.
