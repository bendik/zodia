//! Transit computation — current sky positions interacting with a natal chart.
//!
//! Transits are the primary lens for Zodia's default "now" view:
//!   - Which transiting planets are aspecting natal planets?
//!   - Which natal house is each transiting planet currently moving through?
//!   - What are the aspects among the transiting planets themselves (the sky chart)?
//!
//! Each observed phenomenon has a corresponding `InterpKey` that links it to
//! community interpretations in the store.

use crate::aspects::{Aspect, AspectKind, angular_separation, compute_aspects, detect_aspect};
use crate::interp::InterpKey;
use crate::planet::{Planet, PlanetPositions};
use serde::{Deserialize, Serialize};

/// A transiting planet making an aspect to a natal planet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitAspect {
    pub transiting: Planet,
    pub natal_body: Planet,
    pub kind: AspectKind,
    pub orb: f64,
}

impl TransitAspect {
    pub fn interp_key(&self) -> InterpKey {
        InterpKey::Transit {
            transiting: self.transiting,
            natal_body: self.natal_body,
            kind: self.kind,
        }
    }
}

/// A transiting planet currently occupying a natal house.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HouseTransit {
    pub transiting: Planet,
    /// Natal house number (1–12)
    pub house: u8,
}

impl HouseTransit {
    pub fn interp_key(&self) -> InterpKey {
        InterpKey::HouseTransit {
            transiting: self.transiting,
            house: self.house,
        }
    }
}

/// The full picture of current transits against a natal chart.
///
/// This is what the "now" view is built from.  Every element carries
/// an `interp_key()` method to look up community interpretations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitSet {
    /// JDN at which the sky positions were sampled
    pub transit_jdn: f64,
    /// Current geocentric planetary positions
    pub sky: PlanetPositions,
    /// Aspects from transiting planets to natal planets
    pub transit_aspects: Vec<TransitAspect>,
    /// Which natal house each transiting planet currently occupies
    pub house_transits: Vec<HouseTransit>,
    /// Aspects among the transiting planets themselves (the sky's own chart)
    pub sky_aspects: Vec<Aspect>,
}

impl TransitSet {
    /// All `InterpKey`s relevant to this snapshot — useful for a bulk
    /// interpretation lookup when rendering the "now" view.
    pub fn interp_keys(&self) -> Vec<InterpKey> {
        let mut keys: Vec<InterpKey> = self.transit_aspects.iter()
            .map(TransitAspect::interp_key)
            .collect();
        keys.extend(self.house_transits.iter().map(HouseTransit::interp_key));
        keys
    }
}

// ── window ───────────────────────────────────────────────────────────────────

/// Find the JDN range during which `transiting` occupies the same natal
/// house it occupies at `jdn`.  Scans backward + forward at a per-planet
/// step (~0.5° of motion per step), capped to ~30 years for the outermost
/// bodies.  Returns `(start_jdn, end_jdn)`.
///
/// Falls back to `(jdn, jdn)` if positions can't be computed or the chart
/// has a stub house system.
pub fn house_transit_window(
    transiting: crate::planet::Planet,
    houses:     &crate::houses::HouseSystem,
    jdn:        f64,
) -> (f64, f64) {
    let Some(cur_lon) = crate::ephemeris::compute_positions(jdn).ok()
        .and_then(|p| p.get(transiting)) else { return (jdn, jdn); };
    let cur_house = houses.house_of(cur_lon);
    if cur_house == 0 { return (jdn, jdn); }   // stub system

    let step = planet_step_days(transiting);
    let cap  = planet_cap_days(transiting);

    let in_same_house = |t: f64| -> bool {
        crate::ephemeris::compute_positions(t).ok()
            .and_then(|p| p.get(transiting))
            .map(|lon| houses.house_of(lon) == cur_house)
            .unwrap_or(false)
    };

    let start = {
        let mut t = jdn;
        loop {
            let prev = t - step;
            if jdn - prev > cap { break jdn - cap; }
            if !in_same_house(prev) { break t; }
            t = prev;
        }
    };

    let end = {
        let mut t = jdn;
        loop {
            let next = t + step;
            if next - jdn > cap { break jdn + cap; }
            if !in_same_house(next) { break t; }
            t = next;
        }
    };

    (start, end)
}

/// Days-per-step for the house-window scan — ~0.5° of mean motion per step.
fn planet_step_days(p: crate::planet::Planet) -> f64 {
    use crate::planet::Planet::*;
    match p {
        Moon                          => 0.05,
        Sun | Mercury | Venus         => 0.5,
        Mars                          => 1.0,
        Jupiter                       => 5.0,
        Saturn                        => 15.0,
        Uranus                        => 30.0,
        Neptune                       => 60.0,
        Pluto                         => 90.0,
    }
}

/// Upper bound on the half-window scan.  Set generously so even Pluto's
/// ~15-year-per-house stay is fully captured.
fn planet_cap_days(p: crate::planet::Planet) -> f64 {
    use crate::planet::Planet::*;
    match p {
        Moon                          => 5.0,
        Sun | Mercury | Venus         => 60.0,
        Mars                          => 180.0,
        Jupiter                       => 730.0,
        Saturn                        => 1825.0,    // ~5y
        Uranus                        => 3650.0,    // ~10y
        Neptune                       => 5475.0,    // ~15y
        Pluto                         => 10950.0,   // ~30y
    }
}

/// Find the JDN range during which `transiting` is within the default orb of
/// `kind` aspect to `natal_lon`.
///
/// Scans backward then forward from `jdn` in 1-day steps, capping at ±90 days.
/// Returns `(start_jdn, end_jdn)`.
pub fn transit_window(
    transiting: Planet,
    natal_lon: f64,
    kind: AspectKind,
    jdn: f64,
) -> (f64, f64) {
    let orb_limit = kind.default_orb();

    let in_orb = |t: f64| -> bool {
        crate::ephemeris::compute_positions(t)
            .ok()
            .and_then(|pos| pos.get(transiting))
            .map(|lon| {
                (angular_separation(lon, natal_lon) - kind.angle()).abs() <= orb_limit
            })
            .unwrap_or(false)
    };

    let start = {
        let mut t = jdn;
        loop {
            let prev = t - 1.0;
            if prev < jdn - 90.0 {
                break jdn - 90.0;
            }
            if !in_orb(prev) {
                break t;
            }
            t = prev;
        }
    };

    let end = {
        let mut t = jdn;
        loop {
            let next = t + 1.0;
            if next > jdn + 90.0 {
                break jdn + 90.0;
            }
            if !in_orb(next) {
                break t;
            }
            t = next;
        }
    };

    (start, end)
}

// ── computation ──────────────────────────────────────────────────────────────

/// Aspects from every transiting planet to every natal planet.
///
/// Uses the same orbs as natal aspects; transit orbs can be tightened
/// per tradition by passing a custom `orb_scale` factor in a future revision.
pub fn compute_transit_aspects(
    sky: &PlanetPositions,
    natal: &PlanetPositions,
) -> Vec<TransitAspect> {
    let mut aspects = Vec::new();
    for &t in Planet::all() {
        for &n in Planet::all() {
            let Some(t_lon) = sky.get(t)   else { continue };
            let Some(n_lon) = natal.get(n) else { continue };
            if let Some((kind, orb)) = detect_aspect(angular_separation(t_lon, n_lon)) {
                aspects.push(TransitAspect { transiting: t, natal_body: n, kind, orb });
            }
        }
    }
    aspects
}

/// Build a `TransitSet` from precomputed sky positions and natal data.
pub fn build_transit_set(
    transit_jdn: f64,
    sky: PlanetPositions,
    natal: &PlanetPositions,
    house_transits: Vec<HouseTransit>,
) -> TransitSet {
    let transit_aspects = compute_transit_aspects(&sky, natal);
    let sky_aspects = compute_aspects(&sky);
    TransitSet { transit_jdn, sky, transit_aspects, house_transits, sky_aspects }
}
