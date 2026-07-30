//! Private-circle sharing.
//!
//! A circle IS a `p2panda_spaces::Space` — this crate does not implement
//! group cryptography itself, it wires Zodia's identity and storage into
//! `p2panda-spaces`'s `Manager`/`Space` API. See `docs/prd/circles.md` for
//! why (the parent architecture PRD originally assumed we'd wrap raw
//! group-encryption primitives ourselves; reading the real 0.7.0 API showed
//! `p2panda-spaces` already composes `p2panda-auth` (membership + access
//! levels) and `p2panda-encryption` (the actual key agreement/ratchet) into
//! exactly this concept).
//!
//! `C` (access conditions) is `()` for this first slice — per-path/content
//! access partitioning is real but not needed to share a reading with a
//! named group of people.

use std::borrow::Borrow;

use p2panda_auth::group::GroupCrdtState;
use p2panda_core::traits::{Digest, Provenance};
use p2panda_core::{Hash, Header, Operation, SigningKey, Topic, VerifyingKey};
use p2panda_spaces::manager::Manager;
use p2panda_spaces::space::{Space, SpacesState};
use p2panda_store::groups::GroupsStore;
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::spaces::{SpacesStore, SqliteSpacesStore};
use p2panda_store::topics::TopicStore;
use p2panda_store::{SqliteError, SqliteStore, Transaction};
use thiserror::Error;

pub use p2panda_auth::Access;
pub use p2panda_spaces::{
    AuthMessage, Config, Credentials, Event, SpaceId, SpacesArgs, SpacesStoreState,
    StrongRemoveResolver,
};

/// Access conditions. Trivial for this first slice — see module docs.
pub type CircleConditions = ();

/// The extensions type circle/space operations carry in their `Header` —
/// distinct from `zodia_ops::OpExtensions`, and deliberately so: a circle
/// operation is never a regular `InterpOp`/`DocOp`, it lives on its own
/// per-circle log/topic (`Topic::from(blake3("circle:" || circle_id))` per
/// the parent architecture PRD).
pub type CircleExtensions = SpacesArgs<CircleConditions>;

/// A circle/space operation, ready to sign, persist, and broadcast exactly
/// like any other Zodia operation — just with a different extensions type.
///
/// Wraps `Operation<CircleExtensions>` rather than being a bare type alias
/// because `p2panda_spaces::Forge::Message` requires `Borrow<CircleExtensions>`,
/// which `p2panda_core::Operation<E>` doesn't implement (it only implements
/// `Borrow<Header<E>>`) — the orphan rule means only a local type can carry
/// the extra impl.
#[derive(Debug, Clone)]
pub struct CircleOperation(pub Operation<CircleExtensions>);

impl Borrow<CircleExtensions> for CircleOperation {
    fn borrow(&self) -> &CircleExtensions {
        &self.0.header.extensions
    }
}

impl Digest<Hash> for CircleOperation {
    fn hash(&self) -> Hash {
        self.0.hash()
    }
}

impl Provenance<VerifyingKey> for CircleOperation {
    fn author(&self) -> VerifyingKey {
        self.0.author()
    }

    fn verify(&self) -> bool {
        self.0.verify()
    }
}

pub type CircleResolver = StrongRemoveResolver<CircleConditions>;
pub type CircleSpacesStore = SqliteSpacesStore<CircleExtensions>;
pub type CircleManager = Manager<CircleSpacesStore, ZodiaForge, CircleConditions, CircleResolver>;
pub type CircleSpace = Space<CircleSpacesStore, ZodiaForge, CircleConditions, CircleResolver>;

/// The log id circle operations are appended to. One log per local
/// identity, same as `zodia-sync`'s `INTERP_LOG_ID` convention but on a
/// completely separate log — circle operations (`Operation<CircleExtensions>`)
/// and interpretation operations (`Operation<zodia_ops::OpExtensions>`) are
/// different types and cannot share a log.
pub const CIRCLE_LOG_ID: u64 = 0;

/// This circle's dedicated sync topic (`Topic::from(blake3("circle:" ||
/// circle_id))` per the parent architecture PRD's sketch) — everything
/// exchanged for one circle (its auth/membership messages and its
/// application messages) flows over this one topic, distinct per circle.
pub fn topic_for_circle(circle_id: SpaceId) -> Topic {
    Topic::from(*blake3::hash(&[b"zodia:v1:circle:", circle_id.as_bytes().as_slice()].concat()).as_bytes())
}

/// The well-known topic every device subscribes to so peers can discover
/// each other's `p2panda-encryption` key bundles — the prerequisite for
/// being added to *any* circle. `Manager::key_bundle_message()` produces a
/// signed `SpacesArgs::KeyBundle` op; broadcasting it here and having every
/// peer `process` what they receive is `p2panda-spaces`'s own intended
/// mechanism for member discovery (`Member` itself is deliberately not
/// constructible from raw bytes outside the crate — see `KeyBundle`'s own
/// doc comment — so this op, not a hand-rolled invite handshake, is the
/// real path).
pub fn circle_directory_topic() -> Topic {
    Topic::from(*blake3::hash(b"zodia:v1:circle-directory").as_bytes())
}

/// Persist a received circle operation into the local store before handing
/// it to `Manager::process_persisted`/`process`.
///
/// `p2panda-spaces` looks up an operation's dependencies (e.g. the auth
/// message a space-membership message references) by hash against the
/// store — an operation authored by a peer and never locally persisted is
/// invisible to that lookup (`MissingAuthMessage`), the same reasoning
/// `zodia-sync::store_and_associate` documents for the regular InterpOp
/// receive path. Every circle operation — ours or a peer's — goes through
/// the same `CIRCLE_LOG_ID`.
pub async fn persist_received(
    store: &SqliteStore,
    op: &CircleOperation,
) -> Result<(), CircleError> {
    let permit = store.begin().await?;
    store.insert_operation(&op.0.hash, &op.0, &CIRCLE_LOG_ID).await?;
    store.commit(permit).await?;
    Ok(())
}

/// Same as [`persist_received`], but also `TopicStore::associate`s
/// `(topic, author, CIRCLE_LOG_ID)` in the same transaction — the circle
/// equivalent of `zodia-sync::store_and_associate`, needed so a *third*
/// peer's catch-up query against this device finds the operation (without
/// `associate`, it sits in the store but is invisible to `TopicStore::resolve`,
/// the exact bug `store_and_associate`'s own doc comment describes for the
/// regular InterpOp path). What the sync-layer receive path should call;
/// [`persist_received`] stays as the simpler primitive the crate's own
/// tests use, where no relay/resolve guarantee is being exercised.
pub async fn persist_received_and_associate(
    store:  &SqliteStore,
    topic:  Topic,
    author: &VerifyingKey,
    op:     &CircleOperation,
) -> Result<(), CircleError> {
    let permit = store.begin().await?;
    if let Err(e) = store.insert_operation(&op.0.hash, &op.0, &CIRCLE_LOG_ID).await {
        let _ = store.rollback(permit).await;
        return Err(e.into());
    }
    if let Err(e) = store.associate(&topic, author, &CIRCLE_LOG_ID).await {
        let _ = store.rollback(permit).await;
        return Err(e.into());
    }
    store.commit(permit).await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum CircleError {
    #[error("p2panda store: {0}")]
    Store(#[from] SqliteError),
    #[error("random generation: {0}")]
    Rng(String),
    #[error("identity secret file: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity secret codec: {0}")]
    Codec(String),
    #[error("circle manager: {0}")]
    Manager(String),
}

/// Load this device's persisted circle `identity_secret`, or generate and
/// persist a fresh one if none exists yet.
///
/// `p2panda_spaces::Credentials::from_keys` reuses Zodia's existing
/// `SigningKey` directly (see module docs — "one new secret, not a new
/// identity"), but the `identity_secret` half (an x25519 key used for
/// `p2panda-encryption`'s key agreement) has to be a real one, and
/// `p2panda_encryption::crypto::x25519::SecretKey` can only be constructed
/// via `SecretKey::from_rng` outside the crate's own `test_utils` feature —
/// there's no public deterministic-derivation path, by design (the crate
/// doesn't want callers rolling their own key derivation). So it has to be
/// generated once and persisted, not re-derived from the signing key on
/// every launch — a stable `identity_secret` is required for key agreement
/// with peers to keep working across restarts.
fn load_or_create_identity_secret(
    path: &std::path::Path,
) -> Result<p2panda_encryption::crypto::x25519::SecretKey, CircleError> {
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(secret) = ciborium::de::from_reader(&bytes[..]) {
            return Ok(secret);
        }
    }
    let rng = p2panda_encryption::Rng::default();
    let secret = p2panda_encryption::crypto::x25519::SecretKey::from_rng(&rng)
        .map_err(|e| CircleError::Rng(e.to_string()))?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&secret, &mut buf).map_err(|e| CircleError::Codec(e.to_string()))?;
    std::fs::write(path, buf)?;
    Ok(secret)
}

/// Build the `CircleManager` for this device: loads/creates the persisted
/// `identity_secret`, wires `ZodiaForge` onto the given `SqliteStore`
/// (the same store `zodia-sync` already owns), and constructs the
/// `p2panda_spaces::Manager`.
///
/// `identity_secret_path` should live alongside the sync store's own file
/// (e.g. `store_dir.join("circle_identity_secret.cbor")`) — a small,
/// device-local secret, not something synced or shared.
pub fn new_manager(
    identity_secret_path: &std::path::Path,
    store: SqliteStore,
    signing_key: SigningKey,
) -> Result<CircleManager, CircleError> {
    let identity_secret = load_or_create_identity_secret(identity_secret_path)?;
    let credentials = Credentials::from_keys(signing_key.clone(), identity_secret);
    let spaces_store = CircleSpacesStore::new(store.clone());
    let forge = ZodiaForge::new(store, signing_key);
    let rng = p2panda_encryption::Rng::default();
    CircleManager::new(spaces_store, forge, credentials, rng)
        .map_err(|e| CircleError::Manager(e.to_string()))
}

// ── production-safe persistence wrappers ────────────────────────────────────
//
// `p2panda_spaces::Manager`/`Space` have `_persisted` convenience methods
// (`create_space_persisted`, `add_persisted`, `remove_persisted`,
// `publish_persisted`, `process_persisted`) that do exactly what the plain
// `create_space`/`add`/`remove`/`publish`/`process` do, plus writing the
// resulting state to the store — but the whole `impl` block those
// conveniences live in is gated `#[cfg(any(test, feature = "test_utils"))]`
// in the vendored 0.7.0 source, so they're unavailable to a normal
// production dependency. The plain methods are NOT gated, and neither are
// the underlying `p2panda_store::{GroupsStore, SpacesStore}` trait methods
// they'd otherwise call internally — so this reimplements the same
// persist-after-mutate sequence (confirmed by reading the gated methods'
// own source) using only the ungated surface. Nothing here weakens any
// security property; it's the same store writes the gated wrappers make,
// just made without them.

/// The exact context key `p2panda-spaces`'s own (gated) `get_groups_state`/
/// `set_groups_state` use internally (`Hash::digest(b"global-groups-context")`)
/// — a fixed, hardcoded string in their source, not something generated or
/// exposed, so reproducing it here reads from/writes to the same slot their
/// own ungated methods (`create_space`, `add`, `remove`, `process`, ...)
/// read from internally.
fn global_groups_context_key() -> Hash {
    Hash::digest(b"global-groups-context")
}

type GroupsState = GroupCrdtState<VerifyingKey, Hash, AuthMessage<CircleConditions>, CircleConditions>;

async fn persist_groups_state(store: &SqliteStore, y: &GroupsState) -> Result<(), CircleError> {
    let permit = store.begin().await?;
    if let Err(e) = GroupsStore::set_groups_state_tx(store, global_groups_context_key(), y).await {
        let _ = store.rollback(permit).await;
        return Err(CircleError::Manager(e.to_string()));
    }
    store.commit(permit).await?;
    Ok(())
}

async fn persist_space_state(
    store:    &SqliteStore,
    space_id: SpaceId,
    y:        SpacesState<CircleConditions>,
) -> Result<(), CircleError> {
    let permit = store.begin().await?;
    let store_state: SpacesStoreState<CircleConditions> = y.into();
    let spaces_store = CircleSpacesStore::new(store.clone());
    if let Err(e) = SpacesStore::set_space_state_tx(&spaces_store, &space_id, &store_state).await {
        let _ = store.rollback(permit).await;
        return Err(CircleError::Manager(e.to_string()));
    }
    store.commit(permit).await?;
    Ok(())
}

/// Create a new circle. `store` must be the same `SqliteStore` `manager`
/// was built against (`new_manager`'s caller already has both).
pub async fn create_circle(
    manager:         &CircleManager,
    store:           &SqliteStore,
    id:              SpaceId,
    initial_members: &[(VerifyingKey, Access<CircleConditions>)],
) -> Result<Vec<CircleOperation>, CircleError> {
    let (groups_y, space_y, messages) = manager.create_space(id, initial_members).await
        .map_err(|e| CircleError::Manager(e.to_string()))?;
    persist_groups_state(store, &groups_y).await?;
    persist_space_state(store, space_y.space_id, space_y).await?;
    Ok(messages)
}

/// Grant `member` `access` in `circle_id`.
pub async fn invite_to_circle(
    manager:   &CircleManager,
    store:     &SqliteStore,
    circle_id: SpaceId,
    member:    VerifyingKey,
    access:    Access<CircleConditions>,
) -> Result<(CircleOperation, CircleOperation), CircleError> {
    let space = manager.space(circle_id).await
        .map_err(|e| CircleError::Manager(e.to_string()))?
        .ok_or_else(|| CircleError::Manager("no such circle".into()))?;
    let (groups_y, space_y, auth_message, space_message) = space.add(member, access).await
        .map_err(|e| CircleError::Manager(e.to_string()))?;
    persist_groups_state(store, &groups_y).await?;
    persist_space_state(store, space_y.space_id, space_y).await?;
    Ok((auth_message, space_message))
}

/// Revoke `member`'s access to `circle_id`.
pub async fn revoke_from_circle(
    manager:   &CircleManager,
    store:     &SqliteStore,
    circle_id: SpaceId,
    member:    VerifyingKey,
) -> Result<(CircleOperation, CircleOperation), CircleError> {
    let space = manager.space(circle_id).await
        .map_err(|e| CircleError::Manager(e.to_string()))?
        .ok_or_else(|| CircleError::Manager("no such circle".into()))?;
    let (groups_y, space_y, auth_message, space_message) = space.remove(member).await
        .map_err(|e| CircleError::Manager(e.to_string()))?;
    persist_groups_state(store, &groups_y).await?;
    persist_space_state(store, space_y.space_id, space_y).await?;
    Ok((auth_message, space_message))
}

/// Encrypt and share `plaintext` with every current member of `circle_id`.
pub async fn share_to_circle(
    manager:   &CircleManager,
    store:     &SqliteStore,
    circle_id: SpaceId,
    plaintext: &[u8],
) -> Result<CircleOperation, CircleError> {
    let space = manager.space(circle_id).await
        .map_err(|e| CircleError::Manager(e.to_string()))?
        .ok_or_else(|| CircleError::Manager("no such circle".into()))?;
    let (space_y, message) = space.publish(plaintext).await
        .map_err(|e| CircleError::Manager(e.to_string()))?;
    persist_space_state(store, space_y.space_id, space_y).await?;
    Ok(message)
}

/// Current members of `circle_id` and their access levels.
pub async fn circle_members(
    manager:   &CircleManager,
    circle_id: SpaceId,
) -> Result<Vec<(VerifyingKey, Access<CircleConditions>)>, CircleError> {
    let space = manager.space(circle_id).await
        .map_err(|e| CircleError::Manager(e.to_string()))?
        .ok_or_else(|| CircleError::Manager("no such circle".into()))?;
    space.members().await.map_err(|e| CircleError::Manager(e.to_string()))
}

/// Process a received circle operation (already persisted via
/// [`persist_received_and_associate`]/[`persist_received`]) and persist
/// whatever state it changed — the production-safe equivalent of the
/// gated `Manager::process_persisted`.
pub async fn process_and_persist(
    manager: &CircleManager,
    store:   &SqliteStore,
    op:      &CircleOperation,
) -> Result<Vec<Event<CircleConditions>>, CircleError> {
    let (groups_y, space_y, events) = manager.process(op).await
        .map_err(|e| CircleError::Manager(e.to_string()))?;
    if let Some(groups_y) = groups_y {
        persist_groups_state(store, &groups_y).await?;
    }
    if let Some(space_y) = space_y {
        persist_space_state(store, space_y.space_id, space_y).await?;
    }
    Ok(events)
}

/// Signs and persists `p2panda-spaces` control/application messages using
/// Zodia's own identity and the same `SqliteStore` `zodia-sync` already
/// owns — mirroring `p2panda-spaces`'s own `test_utils::TestForge`, just
/// pointed at production storage instead of a temporary one.
#[derive(Debug, Clone)]
pub struct ZodiaForge {
    signing_key: SigningKey,
    store:       SqliteStore,
}

impl ZodiaForge {
    pub fn new(store: SqliteStore, signing_key: SigningKey) -> Self {
        Self { store, signing_key }
    }
}

impl p2panda_spaces::Forge<CircleConditions> for ZodiaForge {
    type Message = CircleOperation;
    type Error = CircleError;

    fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    async fn forge(&self, args: CircleExtensions) -> Result<Self::Message, Self::Error> {
        let latest: Option<Operation<CircleExtensions>> = self
            .store
            .get_latest_entry(&self.signing_key.verifying_key(), &CIRCLE_LOG_ID)
            .await?;

        let (seq_num, backlink) = match latest {
            Some(prev) => (prev.header.seq_num + 1, Some(prev.header.hash())),
            None => (0, None),
        };

        let mut header = Header::<CircleExtensions> {
            version:       1,
            verifying_key: self.signing_key.verifying_key(),
            signature:     None,
            payload_size:  0,
            payload_hash:  None,
            seq_num,
            backlink,
            extensions:    args,
        };
        header.sign(&self.signing_key);
        let hash: Hash = header.hash();

        let operation = Operation { hash, header, body: None };
        let permit = self.store.begin().await?;
        self.store
            .insert_operation(&hash, &operation, &CIRCLE_LOG_ID)
            .await?;
        self.store.commit(permit).await?;

        Ok(CircleOperation(operation))
    }
}

#[cfg(test)]
mod tests {
    use p2panda_encryption::Rng;
    use p2panda_spaces::Credentials;
    use p2panda_store::SqliteStore;

    use super::*;

    #[test]
    fn identity_secret_round_trips_across_a_restart() {
        let path = std::env::temp_dir().join(format!(
            "zodia-circles-test-identity-secret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        // First "launch": no file yet, one gets created.
        let first = load_or_create_identity_secret(&path).expect("create");
        // Second "launch": the same file must be reused, not regenerated —
        // a device whose identity_secret changed on every restart could
        // never keep key agreement working with peers across a restart.
        let second = load_or_create_identity_secret(&path).expect("load");
        assert_eq!(first, second, "identity_secret must survive a restart unchanged");
        let _ = std::fs::remove_file(&path);
    }

    async fn make_manager(seed: u8) -> (CircleManager, SqliteStore) {
        let rng = Rng::from_seed([seed; 32]);
        let credentials = Credentials::from_rng(&rng).unwrap();
        let store = SqliteStore::temporary().await;
        let spaces_store = CircleSpacesStore::new(store.clone());
        let forge = ZodiaForge::new(store.clone(), credentials.signing_key());
        let manager = CircleManager::new(spaces_store, forge, credentials, rng).unwrap();
        (manager, store)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_circle_member_can_read_what_the_owner_shares() {
        let (alice, _alice_store) = make_manager(0).await;
        let (bob, bob_store) = make_manager(1).await;

        // Key-bundle exchange: required in both directions — Alice needs
        // Bob's bundle to encrypt to him, Bob needs Alice's to decrypt the
        // DCGKA setup message the space-membership message carries.
        alice
            .register_member(&bob.me().await.unwrap())
            .await
            .unwrap();
        bob.register_member(&alice.me().await.unwrap())
            .await
            .unwrap();

        let space_id = p2panda_spaces::SpaceId::digest(b"family-circle");
        let (_, alice_messages) = alice
            .create_space_persisted(space_id, &[(bob.id(), Access::read())])
            .await
            .unwrap();

        for message in &alice_messages {
            persist_received(&bob_store, message).await.unwrap();
            bob.process_persisted(message).await.unwrap();
        }

        let alice_space = alice.space(space_id).await.unwrap().unwrap();
        let shared = alice_space
            .publish_persisted(b"a private reading, just for you two")
            .await
            .unwrap();

        persist_received(&bob_store, &shared).await.unwrap();
        let events = bob.process_persisted(&shared).await.unwrap();
        let Some(Event::Application { data, .. }) = events.first() else {
            panic!("expected an Application event, got: {events:?}");
        };
        assert_eq!(data, b"a private reading, just for you two");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_non_member_cannot_read_a_circle_share() {
        let (alice, _alice_store) = make_manager(0).await;
        let (bob, bob_store) = make_manager(1).await;
        let (carol, carol_store) = make_manager(2).await;

        alice
            .register_member(&bob.me().await.unwrap())
            .await
            .unwrap();
        bob.register_member(&alice.me().await.unwrap())
            .await
            .unwrap();

        let space_id = p2panda_spaces::SpaceId::digest(b"family-circle");
        let (_, alice_messages) = alice
            .create_space_persisted(space_id, &[(bob.id(), Access::read())])
            .await
            .unwrap();
        for message in &alice_messages {
            persist_received(&bob_store, message).await.unwrap();
            bob.process_persisted(message).await.unwrap();

            // Carol sees the same control-plane traffic a relay/observer
            // would (who's in the circle is not secret) — she's just never
            // granted an access level, so she never gets the group secret.
            persist_received(&carol_store, message).await.unwrap();
            carol.process_persisted(message).await.unwrap();
        }

        let alice_space = alice.space(space_id).await.unwrap().unwrap();
        let shared = alice_space
            .publish_persisted(b"a private reading, just for you two")
            .await
            .unwrap();

        // Carol was never added to the space, so she has no way to derive
        // the group secret — processing the (still fully received)
        // encrypted message must not yield a readable event.
        persist_received(&carol_store, &shared).await.unwrap();
        let events = carol.process_persisted(&shared).await.unwrap();
        assert!(
            events.is_empty(),
            "non-member decrypted a circle share: {events:?}"
        );
    }

    /// The crate's own production entry points (`create_circle`,
    /// `invite_to_circle`, `share_to_circle`, `process_and_persist`) rather
    /// than the test-only `_persisted` sugar the tests above use — proving
    /// the hand-written persistence (`persist_groups_state`/
    /// `persist_space_state`, reproducing what the gated `set_groups_state`/
    /// `set_space_state` do, since that whole convenience layer is
    /// `#[cfg(any(test, feature = "test_utils"))]` in the vendored source
    /// and unavailable to a real dependency) is actually equivalent, not
    /// just plausible from reading their source.
    #[tokio::test(flavor = "current_thread")]
    async fn production_path_without_persisted_sugar_still_shares_correctly() {
        let (alice, alice_store) = make_manager(10).await;
        let (bob, bob_store) = make_manager(11).await;

        alice.register_member(&bob.me().await.unwrap()).await.unwrap();
        bob.register_member(&alice.me().await.unwrap()).await.unwrap();

        let space_id = p2panda_spaces::SpaceId::digest(b"production-path-circle");
        let messages = create_circle(&alice, &alice_store, space_id, &[(bob.id(), Access::read())])
            .await
            .unwrap();

        for message in &messages {
            persist_received(&bob_store, message).await.unwrap();
            process_and_persist(&bob, &bob_store, message).await.unwrap();
        }

        let shared = share_to_circle(&alice, &alice_store, space_id, b"produced without _persisted sugar")
            .await
            .unwrap();

        persist_received(&bob_store, &shared).await.unwrap();
        let events = process_and_persist(&bob, &bob_store, &shared).await.unwrap();
        let Some(Event::Application { data, .. }) = events.first() else {
            panic!("expected an Application event, got: {events:?}");
        };
        assert_eq!(data, b"produced without _persisted sugar");
    }
}
