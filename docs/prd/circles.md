# PRD: zodia-circles — private-circle sharing

**Status:** drafted, not implemented
**Branch:** `main`
**Foundation required:** `docs/prd/p2panda-0.7-migration.md` (ships `p2panda-spaces`, `p2panda-auth`, `p2panda-encryption`, and p2panda-stream's `spaces` processor — all load-bearing here)

## Problem Statement

User stories 9 and 10 from `docs/prd/operations-and-streams-rearchitecture.md`:

> 9. As a Zodia user who shared an interpretation in a private friend circle, I want only members of that circle to be able to read it, so that I can share intimate readings without publishing to everyone.
>
> 10. As a Zodia user, I want to revoke a friend's access to my private circle and rotate the circle's key, so that I retain control of who reads my private contributions over time.

Today sharing is all-or-nothing public. The parent PRD's own sketch for this ("a new `zodia-circles` crate... wraps p2panda 0.6's group-encryption primitives") turned out to rest on a crate that doesn't exist at 0.6.x. Having done the 0.7 upgrade, the real primitives are `p2panda-auth` (group membership + per-member access levels), `p2panda-encryption` (the actual message/key-agreement crypto, with post-compromise security and optional forward secrecy), and `p2panda-spaces` (the crate that composes both into exactly the "space" concept a circle needs — its own description is literally "data encryption for multi-device groups").

## Solution

`zodia-circles` is a thin wrapper around `p2panda_spaces::{Manager, Space}`, not a reimplementation of group crypto. Confirmed by reading the actual 0.7.0 source (not assumed from crate names):

- **A circle IS a `Space`.** `Space::add`/`Space::remove` manage membership; `Space::members()` lists `(MemberId, Access<C>)`; `Space::publish(...)` encrypts an application message to the current group secret, all peers with at least `Read` access can decrypt.
- **Access levels come from `p2panda-auth`** (`Pull`/`Read`/`Write`/`Manage`), not something we invent. A circle owner has `Manage`; an invited friend gets `Read` (or `Write` if we want circles to support replies later — v1 only needs `Read`).
- **Key rotation on revocation is automatic**, not something `zodia-circles` implements. `Space::remove` triggers `p2panda-encryption`'s own group-secret rotation internally (confirmed via `p2panda-spaces`'s `repair_spaces`/`key_bundle_expired` machinery) — the parent PRD's assumption that we'd need to hand-roll "revoke + rotate" was wrong; it's a property of the library.
- **Storage needs nothing new.** `p2panda-store` 0.7.0 ships `groups_v1`, `key_registry_v1`, `key_secrets_v1`, and `spaces_v1` tables and implements `GroupsStore`/`KeyRegistryStore`/`KeySecretsStore`/`SpacesStore` directly on `SqliteStore` — the same `SqliteStore` instance `zodia-sync` already owns. No second database file, no schema we maintain ourselves.
- **Identity needs one new secret, not a new identity.** `p2panda_spaces::Credentials::from_keys(signing_key, identity_secret)` accepts our existing Zodia `SigningKey` directly; only `identity_secret` (an x25519 `SecretKey` used for key agreement) is new, generated once per device and persisted alongside the existing identity material in `zodia-crypto`.
- **The Pipeline gets a new stage, not a new pipeline.** `p2panda-stream` 0.7.0 ships a `spaces::Spaces` processor — a working `Processor<T>` impl for exactly this. It slots into `zodia-pipeline`'s existing processor chain for circle-topic traffic, decrypting `SpacesMessage`s before whatever comes next, the same way `DecodeProcessor` sits at the head of the existing InterpOp/DocOp chain.
- **Circle traffic is its own operation stream**, confirmed by `p2panda-spaces`'s own `Forge` pattern: a circle/space operation's extensions slot holds `SpacesArgs<C>` (`Operation<SpacesArgs<C>>`), not `zodia_ops::OpExtensions` — it cannot share a log with regular `InterpOp`/`DocOp` traffic (different `Operation<E>` at the type level, not just a routing choice). Matches the parent PRD's `Topic::from(blake3("circle:" || circle_id))` sketch — one log/topic per circle, separate from per-key and global topics.
- **`C` (access conditions) starts as `()`.** `p2panda-spaces`'s own `TestConditions = ()` is the minimal instantiation. Per-path/per-content access partitioning (the parent PRD's throwaway example: restricting `Read` to a path) is real but not needed for "share this reading with these five friends" — deferred, not designed around speculatively.
- **Concurrency resolution uses the provided `StrongRemoveResolver<C>`**, not a custom `Resolver` impl. p2panda-auth's own docs describe this as the "cautious" default (a member removed concurrently with new operations they authored has those operations invalidated) — the right default for a friends-and-family circle, and not something to second-guess without a concrete case that needs different semantics.

## User Stories

Reusing 9 and 10 verbatim from the parent PRD (Problem Statement above), plus:

3. As a Zodia developer, I want circle membership and key rotation implemented by calling into `p2panda-spaces` directly, not by hand-rolling group cryptography, so that Zodia's security properties are only as strong as its own code where that code is unavoidable, and inherited from a maintained library everywhere else.

## Implementation Decisions (draft — to be refined during BDD)

### `zodia-circles` crate shape

A new crate owning:

- `CircleManager` — thin wrapper over `p2panda_spaces::Manager<SqliteSpacesStore<CircleExtensions>, ZodiaForge, (), StrongRemoveResolver<()>>`, constructed once per `ZodiaClient` alongside the existing `ZodiaSyncNode`, sharing its `SqliteStore`.
- `ZodiaForge` — our `Forge<()>` impl, mirroring `p2panda-spaces`'s own `test_utils::TestForge`: signs a `Header<SpacesArgs<()>>` with the local `SigningKey`, persists via `OperationStore::insert_operation` against a per-circle log id, same shape `zodia-sync`'s `store_and_associate` already uses for regular ops — the two should probably share code rather than duplicate the sign-and-persist sequence.
- Circle-facing API: `create_circle(name) -> CircleId`, `invite(circle_id, peer) `, `revoke(circle_id, peer)`, `share_to_circle(circle_id, InterpOp::Author{..})`, `members(circle_id)`. Exact shape TBD during BDD — this PRD fixes the *primitives underneath*, not the final Rust API, matching how `zodia-sdk.md` was drafted before its command surface was finalised.

### Wiring into `zodia-pipeline`

Add a `spaces::Spaces` processor stage ahead of (or composed with) `DecodeProcessor`, gated to circle topics only — global/per-key topics keep flowing through the existing chain unchanged. Decrypted `SpacesMessage` application payloads still need decoding into `InterpOp` afterward (a circle-shared interpretation is still an `InterpOp::Author`, just arriving via a decrypt step first) — needs a concrete design pass on whether that's a second processor layer or folded into one.

### "Share with..." UI

Per the parent PRD: a picker on each contribution choosing public network (current behaviour) vs. a named circle. Out of scope for this PRD's drafting phase — follows once the crate/pipeline plumbing above is real and testable via cucumber, matching this project's established BDD-outside-in sequencing (crate + pipeline first, driven by a failing scenario; UI last).

## Testing Decisions

Same model as `granular-topic-subscription.md`/`zodia-sdk.md`: unit tests for `zodia-circles` against `SqliteStore::temporary()` with no live network (`p2panda-spaces`'s own `test_utils::TestPeer` pattern is the template), then a real-network cucumber scenario in `sdk/tests/features/` proving the actual user-visible guarantee — a circle member reads a shared interpretation, a non-member does not, and a revoked member can't read anything published after revocation. No mocking of the crypto layer; if `p2panda-encryption`'s primitives have rough edges (the parent PRD's own risk callout), a real round-trip test is what surfaces them.

## Out of Scope

- Final `zodia-circles` public API surface (deferred to implementation, per above).
- Per-path/content access conditions (`C` beyond `()`).
- Multi-device identity sharing — per the parent PRD's own exclusion, each device keeps its own identity + its own new `identity_secret`; a device joining a circle a friend already invited "you" on a different device to is a separate, unaddressed problem.
- `zodia-channels` (the other untouched Phase D piece) — unrelated, no shared code.
- UI ("Share with..." picker) — follows once the underlying crate is real.

## Further Notes

**Why this PRD is thinner than `pruning.md` or `granular-topic-subscription.md`.** Those PRDs were written with the implementation essentially fully designed, because the primitives involved (SQL DELETE, per-key topic derivation) were already well understood from existing code. `p2panda-spaces` is new to this codebase and larger than either of those features — Manager/Space/Forge/Credentials/Config/Resolver is a wider API than a first read fully resolves. This PRD is deliberately scoped to *confirm the primitives are real and fit* (they are, closely) rather than pre-design every method signature; the BDD cycle that follows is where the remaining design questions (pipeline composition, the final circle-facing API) get resolved against a failing test, the same way every other feature this cycle has been built.
