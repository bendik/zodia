//! `FeedItem` — the unified event type the activity feed renders.
//!
//! Two sources merge into one ordered stream:
//!   * pipeline `StateEvent`s (network ops decoded by `zodia-pipeline`).
//!   * locally-computed transit ticks (a planet entering or leaving orb of
//!     a natal-chart aspect).
//!
//! Each item carries a stable `event_id` so dedup works across restarts and
//! across the synth-on-open path that backfills from `ZodiaStore::recent_feed_rows`.

use p2panda_core::VerifyingKey;
use zodia_pipeline::StateEvent;
use zodia_store::{FeedRow, FeedRowKind};

/// One renderable feed item.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    /// Stable id used for dedup and read-state.
    /// - Op-sourced events: the op hash.
    /// - Transit events: `blake3("transit" || transit_key || enter|leave || jdn_bucket)`.
    pub event_id:   [u8; 32],
    /// Unix timestamp (seconds) used for sort order.  Op events use the
    /// p2panda header's timestamp; transit events use the JDN-derived time.
    pub timestamp:  u64,
    /// True iff this event affects content the local identity authored.
    /// Bell badging consults this field; transit events never target.
    pub targets_me: bool,
    pub payload:    FeedPayload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedPayload {
    InterpAuthored {
        author:     VerifyingKey,
        interp_key: String,
        body:       String,
    },
    AffirmAdded {
        target_log_id: [u8; 32],
        target_key:    String,
        voter:         VerifyingKey,
    },
    ResponseAdded {
        author:        VerifyingKey,
        parent_log_id: [u8; 32],
        interp_key:    String,
        body:          String,
    },
    /// Personal transit currently in orb.  `start_jdn` / `end_jdn` mark the
    /// window during which the aspect is within default orb of the natal
    /// position.  `orb` is the current orb at tick time.  `baseline` is the
    /// bundled interpretation text for this key (None when no baseline
    /// exists — the card then prompts the user to contribute).
    TransitActive {
        transit_key: String,
        start_jdn:   f64,
        end_jdn:     f64,
        orb_deg:     f64,
        baseline:    Option<String>,
    },
    /// Global sky-aspect currently active.  Window is computed against the
    /// other transiting body's current position (symmetric to natal-window).
    SkyAspectActive {
        aspect_key: String,
        start_jdn:  f64,
        end_jdn:    f64,
        orb_deg:    f64,
        baseline:   Option<String>,
    },
    /// House transit: which natal house a transiting planet currently
    /// occupies.  Houses change as the planet moves through degrees of
    /// the zodiac; the window represents the planet's stay in that house.
    HouseTransitActive {
        transit_key: String,
        start_jdn:   f64,
        end_jdn:     f64,
        baseline:    Option<String>,
    },

    // ── collaborative-doc events (Phase F-collab) ────────────────────────────

    /// Someone edited the community doc on a key.
    DocEdited {
        editor:     VerifyingKey,
        interp_key: String,
    },
    /// Your edit on this key was just modified by another peer.  Bell-targets.
    BlockYouAuthoredWasEdited {
        editor:     VerifyingKey,
        interp_key: String,
        edit_op_id: [u8; 32],
    },
    /// A veto applied — doc rolled back on this key.
    DocRolledBack {
        revoker:    VerifyingKey,
        interp_key: String,
    },
    /// Affirmation against (interp_key, revision).
    DocAffirmed {
        voter:      VerifyingKey,
        interp_key: String,
    },
    /// A peer joined or left an editor session.
    EditorPresenceChanged {
        peer:       VerifyingKey,
        interp_key: String,
        joined:     bool,
    },
}

impl FeedItem {
    /// Build a `BlockYouAuthoredWasEdited` item.  Bell-targeting; surfaces
    /// when a peer's edit landed on a block whose ring contains the local
    /// identity, so you can review + veto within the 7-day window.
    pub fn block_you_authored_was_edited(
        interp_key: &str,
        editor:     &[u8; 32],
        edit_op_id: &[u8; 32],
        timestamp:  u64,
    ) -> FeedItem {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"block_you_authored_was_edited");
        hasher.update(edit_op_id);
        let event_id = *hasher.finalize().as_bytes();
        let editor_vk = VerifyingKey::try_from(editor.as_slice())
            .unwrap_or_else(|_| zero_key());
        FeedItem {
            event_id,
            timestamp,
            targets_me: true,
            payload: FeedPayload::BlockYouAuthoredWasEdited {
                editor:     editor_vk,
                interp_key: interp_key.to_string(),
                edit_op_id: *edit_op_id,
            },
        }
    }

    /// Build a `DocRolledBack` item.  `event_id` derives from the vetoed
    /// edit's op id + revoker so a successful veto dedupes per (target,
    /// revoker) pair across restarts.
    pub fn doc_rolled_back(
        interp_key:    &str,
        _edit_author:  &[u8; 32],
        revoker:       &[u8; 32],
        edit_op_id:    &[u8; 32],
        timestamp:     u64,
    ) -> FeedItem {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"doc_rolled_back");
        hasher.update(edit_op_id);
        hasher.update(revoker);
        let event_id = *hasher.finalize().as_bytes();
        let revoker_vk = VerifyingKey::try_from(revoker.as_slice())
            .unwrap_or_else(|_| zero_key());
        FeedItem {
            event_id,
            timestamp,
            targets_me: false,
            payload: FeedPayload::DocRolledBack {
                revoker:    revoker_vk,
                interp_key: interp_key.to_string(),
            },
        }
    }

    /// Own doc edit, pushed locally right after publish so it shows up in
    /// the author's own feed too (mirrors `doc_rolled_back` — the pipeline
    /// only echoes `DocEdited` back from peers, never to the editor
    /// themself, so without this a self-edit was invisible in one's own
    /// feed and thus unreachable from the circle-share button).
    pub fn doc_edited(interp_key: &str, editor: &[u8; 32], edit_op_id: &[u8; 32], timestamp: u64) -> FeedItem {
        let editor_vk = VerifyingKey::try_from(editor.as_slice())
            .unwrap_or_else(|_| zero_key());
        FeedItem {
            event_id:   *edit_op_id,
            timestamp,
            targets_me: false,
            payload: FeedPayload::DocEdited {
                editor:     editor_vk,
                interp_key: interp_key.to_string(),
            },
        }
    }

    /// `interp_key` predicate used by per-aspect-page filtering.  Returns the
    /// interp_key the event is about, where applicable.
    pub fn interp_key(&self) -> Option<&str> {
        match &self.payload {
            FeedPayload::InterpAuthored { interp_key, .. } => Some(interp_key),
            FeedPayload::AffirmAdded    { target_key,  .. } => Some(target_key),
            FeedPayload::ResponseAdded  { interp_key, .. } => Some(interp_key),
            FeedPayload::TransitActive      { transit_key, .. } => Some(transit_key),
            FeedPayload::SkyAspectActive    { aspect_key,  .. } => Some(aspect_key),
            FeedPayload::HouseTransitActive { transit_key, .. } => Some(transit_key),
            FeedPayload::DocEdited                  { interp_key, .. } => Some(interp_key),
            FeedPayload::BlockYouAuthoredWasEdited  { interp_key, .. } => Some(interp_key),
            FeedPayload::DocRolledBack              { interp_key, .. } => Some(interp_key),
            FeedPayload::DocAffirmed                { interp_key, .. } => Some(interp_key),
            FeedPayload::EditorPresenceChanged      { interp_key, .. } => Some(interp_key),
        }
    }
}

// ── conversions ───────────────────────────────────────────────────────────────

/// Convert a pipeline `StateEvent` into a `FeedItem`.  The caller supplies
/// `me` (local pubkey bytes), the interp_key (looked up from the store for
/// `AffirmAdded` / `ResponseAdded`), and whether the event targets the local
/// identity (also a store lookup for affirms/responses).
///
/// Returns `None` for `StateEvent::Skipped` — those don't render.
pub fn state_event_to_feed_item(
    event:        &StateEvent,
    timestamp:    u64,
    interp_key:   String,
    targets_me:   bool,
) -> Option<FeedItem> {
    Some(match event {
        StateEvent::InterpAuthored { op_id, author, interp_key: key, body } => FeedItem {
            event_id:   *op_id.as_bytes(),
            timestamp,
            targets_me,
            payload:    FeedPayload::InterpAuthored {
                author:     *author,
                interp_key: key.clone(),
                body:       body.clone(),
            },
        },
        StateEvent::AffirmAdded { target_log_id, voter } => {
            // event_id for affirms is the affirmation log_id (derived
            // downstream as BLAKE3(interp_log_id || voter)).  We use the
            // op hash here when available; the store synth path uses the
            // real affirmations.log_id.  For pipeline-sourced affirms, we
            // pack target_log_id as a placeholder — the dedup field that
            // matters for the live stream is voter+target.
            //
            // The store's `feed_targeting_unread_*` queries key on the
            // affirmation row's log_id, not on this event_id, so the bell
            // count remains correct regardless.
            let mut hasher = blake3::Hasher::new();
            hasher.update(target_log_id.as_bytes());
            hasher.update(voter.as_bytes());
            let event_id = *hasher.finalize().as_bytes();
            FeedItem {
                event_id,
                timestamp,
                targets_me,
                payload: FeedPayload::AffirmAdded {
                    target_log_id: *target_log_id.as_bytes(),
                    target_key:    interp_key,
                    voter:         *voter,
                },
            }
        }
        StateEvent::ResponseAdded { op_id, parent_log_id, author, body } => FeedItem {
            event_id:   *op_id.as_bytes(),
            timestamp,
            targets_me,
            payload:    FeedPayload::ResponseAdded {
                author:        *author,
                parent_log_id: *parent_log_id.as_bytes(),
                interp_key,
                body:          body.clone(),
            },
        },
        StateEvent::Skipped { .. } => return None,
        StateEvent::InterpRevoked { .. } => return None,
        // Handled directly in app.rs's dispatch (store write + in-memory
        // name map); not a feed-worthy event on its own.
        StateEvent::DisplayNameSet { .. } => return None,
        // Handled directly in app.rs's dispatch (toast queue); not a
        // feed-worthy event, same reasoning as DisplayNameSet.
        StateEvent::CircleInviteReceived { .. } => return None,
        StateEvent::DocEdited { by, interp_key: key, .. } => FeedItem {
            event_id:   match event {
                StateEvent::DocEdited { op_id, .. } => *op_id.as_bytes(),
                _ => unreachable!(),
            },
            timestamp,
            targets_me,
            payload: FeedPayload::DocEdited { editor: *by, interp_key: key.clone() },
        },
        StateEvent::DocVetoProposed { .. } => return None,  // app handler resolves
        StateEvent::DocAffirmed { by, interp_key: key, op_id, .. } => FeedItem {
            event_id:   *op_id.as_bytes(),
            timestamp,
            targets_me,
            payload: FeedPayload::DocAffirmed { voter: *by, interp_key: key.clone() },
        },
        StateEvent::EditorPresenceChanged { by, interp_key: key, joined, op_id, .. } =>
            FeedItem {
                event_id:   *op_id.as_bytes(),
                timestamp,
                targets_me,
                payload: FeedPayload::EditorPresenceChanged {
                    peer:       *by,
                    interp_key: key.clone(),
                    joined:     *joined,
                },
            },
    })
}

/// Convert a stored `FeedRow` (from `ZodiaStore::recent_feed_rows`) into a
/// `FeedItem`.  Caller supplies whether the row targets the local identity —
/// this is derivable from the row's `author_is_me` plus join data the store
/// query already filtered on, so for synth we recompute conservatively here:
/// targets_me iff (affirm or response) AND parent/target authored by `me`,
/// which we approximate via the caller having already queried.  In practice
/// the `targets_me` flag is set by the loading code, not here.
pub fn feed_row_to_feed_item(row: &FeedRow, targets_me: bool) -> FeedItem {
    let payload = match row.kind {
        FeedRowKind::Authored => FeedPayload::InterpAuthored {
            author:     row.author_pk
                .and_then(|bytes| VerifyingKey::try_from(bytes.as_slice()).ok())
                .unwrap_or_else(zero_key),
            interp_key: row.interp_key.clone(),
            body:       row.body.clone(),
        },
        FeedRowKind::Affirm => FeedPayload::AffirmAdded {
            target_log_id: row.target_log_id.unwrap_or([0u8; 32]),
            target_key:    row.interp_key.clone(),
            voter:         row.author_pk
                .and_then(|bytes| VerifyingKey::try_from(bytes.as_slice()).ok())
                .unwrap_or_else(zero_key),
        },
        FeedRowKind::Response => FeedPayload::ResponseAdded {
            author:        row.author_pk
                .and_then(|bytes| VerifyingKey::try_from(bytes.as_slice()).ok())
                .unwrap_or_else(zero_key),
            parent_log_id: row.parent_log_id.unwrap_or([0u8; 32]),
            interp_key:    row.interp_key.clone(),
            body:          row.body.clone(),
        },
    };
    FeedItem {
        event_id:   row.event_id,
        timestamp:  row.ts,
        targets_me,
        payload,
    }
}

/// A 32-byte zero key used as a fallback when an author_pk is missing or
/// malformed.  Renders as a placeholder pubkey-tag in UI.
fn zero_key() -> VerifyingKey {
    VerifyingKey::try_from([0u8; 32].as_slice()).expect("zero key is valid")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interp_key_predicate() {
        let item = FeedItem {
            event_id:   [0u8; 32],
            timestamp:  0,
            targets_me: false,
            payload:    FeedPayload::TransitActive {
                transit_key: "transit:venus_trine_sun".into(),
                start_jdn:   2_460_000.5,
                end_jdn:     2_460_010.0,
                orb_deg:     0.5,
                baseline:    None,
            },
        };
        assert_eq!(item.interp_key(), Some("transit:venus_trine_sun"));
    }
}
