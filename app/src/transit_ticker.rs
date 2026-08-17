//! Local transit event source.
//!
//! Diffs the current in-orb transit-aspect set against the previous tick;
//! emits `TransitEnteredOrb` / `TransitLeftOrb` `FeedItem`s for changes.
//! The previous set persists in `feed_meta` so a restart doesn't re-emit
//! every active transit.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{trace, warn};
use zodia_core::Chart;
use zodia_store::{BaselineStore, ZodiaStore};

use crate::aspect_view::resolve_top_body;
use crate::feed_item::{FeedItem, FeedPayload};

/// Default tick interval — 10 minutes per the PRD.  Matches the cadence
/// human-perceptible orb changes happen at: a planet doesn't enter/leave
/// orb in seconds.
const TICK_INTERVAL: Duration = Duration::from_secs(600);

/// Spawn a background task that ticks every `TICK_INTERVAL` and emits
/// `FeedItem`s for transit aspects entering/leaving orb against `chart`.
///
/// Returned `JoinHandle` lets the caller cancel the task on chart change
/// or shutdown; the channel half lives inside the spawned task and closes
/// when the receiver drops.
pub fn spawn(
    chart:    Arc<Chart>,
    baseline: Arc<BaselineStore>,
    store:    ZodiaStore,
    me:       [u8; 32],
    out:      mpsc::Sender<FeedItem>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let me_vk = zodia_doc::VerifyingKey::from_bytes(&me).ok();
        loop {
            tick(&chart, &store, &baseline, me_vk.as_ref(), &out).await;
            tokio::time::sleep(TICK_INTERVAL).await;
        }
    })
}

/// One transit-ticker iteration.  Computes both the personal in-orb set
/// (transiting planets vs natal chart) and the global sky-aspect in-orb
/// set (transiting planets vs each other), diffs both against their
/// respective `prev` sets, and emits the appropriate `FeedItem` variants.
async fn tick(
    chart:    &Chart,
    store:    &ZodiaStore,
    baseline: &BaselineStore,
    me_vk:    Option<&zodia_doc::VerifyingKey>,
    out:      &mpsc::Sender<FeedItem>,
) {
    let items = active_items(chart, store, baseline, me_vk).await;
    trace!(count = items.len(), "transit_ticker tick");
    for item in items {
        let _ = out.send(item).await;
    }
}

/// Compute every currently in-orb transit/sky-aspect/house-transit as a
/// `FeedItem`, each with its reading body resolved DB-first (community
/// reading), baseline-fallback — via the same `resolve_top_body` the aspect
/// list/detail views use, so the Sky feed never shows stale "no reading
/// yet" text for a key someone has actually written about.
async fn active_items(
    chart:    &Chart,
    store:    &ZodiaStore,
    baseline: &BaselineStore,
    me_vk:    Option<&zodia_doc::VerifyingKey>,
) -> Vec<FeedItem> {
    use zodia_core::transit_window;

    let now_jdn = current_jdn();
    let transit_set = match chart.transits_at(now_jdn) {
        Ok(t)  => t,
        Err(e) => {
            warn!("transit_ticker: compute failed: {e}");
            return Vec::new();
        }
    };

    // Sky shows "what is valid right now."  Each tick re-emits an Active
    // event for every in-orb transit.  Deduplication happens downstream at
    // the FeedView (stable event_id keyed off the transit key + window
    // start day).  This sidesteps the prior bug where a user who had
    // already "seen" transits had them suppressed forever by the prev-set
    // gate — meaning Sky stayed empty for returning users.

    let mut items = Vec::new();

    // ── personal (transit→natal aspects with computed window) ───────────────
    for ta in &transit_set.transit_aspects {
        let key = ta.interp_key().to_sig();
        let Some(natal_lon) = chart.positions.get(ta.natal_body) else { continue };
        let (start_jdn, end_jdn) = transit_window(
            ta.transiting, natal_lon, ta.kind, now_jdn,
        );
        let baseline_body = Some(resolve_top_body(store, baseline, me_vk, &ta.interp_key()).await);
        items.push(build_personal_active(key, start_jdn, end_jdn, ta.orb, baseline_body));
    }

    // ── global sky aspects ──────────────────────────────────────────────────
    for a in &transit_set.sky_aspects {
        let key = format!("sky:{}", a.sig());
        let Some(b_lon) = transit_set.sky.get(a.body_b) else { continue };
        let (start_jdn, end_jdn) = transit_window(
            a.body_a, b_lon, a.kind, now_jdn,
        );
        let sky_key = zodia_core::InterpKey::SkyAspect { aspect_sig: a.sig() };
        let baseline_body = Some(resolve_top_body(store, baseline, me_vk, &sky_key).await);
        items.push(build_global_active(key, start_jdn, end_jdn, a.orb, baseline_body));
    }

    // ── house transits ──────────────────────────────────────────────────────
    for ht in &transit_set.house_transits {
        let key = ht.interp_key().to_sig();
        let (start_jdn, end_jdn) = zodia_core::house_transit_window(
            ht.transiting, &chart.houses, now_jdn,
        );
        let baseline_body = Some(resolve_top_body(store, baseline, me_vk, &ht.interp_key()).await);
        items.push(build_house_active(key, start_jdn, end_jdn, baseline_body));
    }

    items
}

/// The currently-active `FeedItem` (if any) whose canonical key signature
/// matches `target_sig` — used to push an immediate Sky-feed refresh right
/// after a reading is published, rather than waiting up to `TICK_INTERVAL`
/// for the next scheduled tick to pick it up.
pub(crate) async fn active_item_for_key(
    chart:      &Chart,
    store:      &ZodiaStore,
    baseline:   &BaselineStore,
    me_vk:      Option<&zodia_doc::VerifyingKey>,
    target_sig: &str,
) -> Option<FeedItem> {
    active_items(chart, store, baseline, me_vk).await
        .into_iter()
        .find(|item| item.interp_key() == Some(target_sig))
}

fn build_personal_active(
    transit_key: String,
    start_jdn:   f64,
    end_jdn:     f64,
    orb_deg:     f64,
    baseline:    Option<String>,
) -> FeedItem {
    let event_id = stable_event_id("transit", &transit_key, true, start_jdn);
    FeedItem {
        event_id,
        timestamp:  jdn_to_unix_inverted(end_jdn),
        targets_me: false,
        payload: FeedPayload::TransitActive {
            transit_key, start_jdn, end_jdn, orb_deg, baseline,
        },
    }
}

fn build_global_active(
    aspect_key: String,
    start_jdn:  f64,
    end_jdn:    f64,
    orb_deg:    f64,
    baseline:   Option<String>,
) -> FeedItem {
    let event_id = stable_event_id("sky", &aspect_key, true, start_jdn);
    FeedItem {
        event_id,
        timestamp:  jdn_to_unix_inverted(end_jdn),
        targets_me: false,
        payload: FeedPayload::SkyAspectActive {
            aspect_key, start_jdn, end_jdn, orb_deg, baseline,
        },
    }
}

fn build_house_active(
    transit_key: String,
    start_jdn:   f64,
    end_jdn:     f64,
    baseline:    Option<String>,
) -> FeedItem {
    let event_id = stable_event_id("house", &transit_key, true, start_jdn);
    FeedItem {
        event_id,
        timestamp:  jdn_to_unix_inverted(end_jdn),
        targets_me: false,
        payload: FeedPayload::HouseTransitActive {
            transit_key, start_jdn, end_jdn, baseline,
        },
    }
}

/// Map an end-of-orb JDN to a descending sort key: soon-ending = high,
/// far-ending = low.  Anchored to "today" so the sort interleaves nicely
/// with op events whose sort key is their unix author ts.
fn jdn_to_unix_inverted(end_jdn: f64) -> u64 {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let end_unix = jdn_to_unix(end_jdn) as i64;
    // `score = now - (end - now)` => closer-to-end → higher score.
    let score = (2 * now_unix) - end_unix;
    score.max(0) as u64
}

/// Stable event id: `blake3(domain || key || direction || jdn_floor_day)`.
/// Bucketing at the day level (not minute) makes the id stable across
/// re-ticks within the same UTC day even if `transit_window`'s scan picks
/// up a slightly different start_jdn fraction.
fn stable_event_id(domain: &str, key: &str, entered: bool, jdn: f64) -> [u8; 32] {
    let direction = if entered { "enter" } else { "leave" };
    let jdn_day = jdn.floor() as i64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(key.as_bytes());
    hasher.update(direction.as_bytes());
    hasher.update(&jdn_day.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Convert a Julian-day number to a Unix epoch seconds value.  JDN
/// 2440587.5 = 1970-01-01 00:00 UTC; one day = 86 400 seconds.
fn jdn_to_unix(jdn: f64) -> u64 {
    let secs = (jdn - 2_440_587.5) * 86_400.0;
    secs.max(0.0) as u64
}

fn current_jdn() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    secs / 86_400.0 + 2_440_587.5
}


// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_event_id_within_same_day() {
        // Two jdns within the same UTC day → same event id.
        let jdn_a = 2_460_000.1;
        let jdn_b = 2_460_000.9;
        let ev_a = build_personal_active("transit:x".into(), jdn_a, jdn_a + 5.0, 0.5, None);
        let ev_b = build_personal_active("transit:x".into(), jdn_b, jdn_b + 5.0, 0.5, None);
        assert_eq!(ev_a.event_id, ev_b.event_id);
    }
}
