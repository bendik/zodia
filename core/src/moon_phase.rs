//! Moon phase — one of the most universally recognized astrology/astronomy
//! concepts, and previously entirely absent from this app despite computing
//! both Sun and Moon longitude already.
//!
//! Phase is purely a function of the Sun-Moon angular separation (the
//! "phase angle"): 0° is New Moon, 90° First Quarter, 180° Full Moon, 270°
//! Last Quarter, with the four intermediate names covering the 45° segments
//! between them. No new ephemeris data needed — `compute_positions` already
//! gives us both bodies' longitudes.

use crate::ephemeris::{compute_positions, EphemerisError};
use crate::planet::Planet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoonPhase {
    New,
    WaxingCrescent,
    FirstQuarter,
    WaxingGibbous,
    Full,
    WaningGibbous,
    LastQuarter,
    WaningCrescent,
}

impl MoonPhase {
    /// Plain-English name, e.g. "New Moon", "Waxing Crescent".
    pub fn name(&self) -> &'static str {
        match self {
            MoonPhase::New            => "New Moon",
            MoonPhase::WaxingCrescent  => "Waxing Crescent",
            MoonPhase::FirstQuarter    => "First Quarter",
            MoonPhase::WaxingGibbous   => "Waxing Gibbous",
            MoonPhase::Full            => "Full Moon",
            MoonPhase::WaningGibbous   => "Waning Gibbous",
            MoonPhase::LastQuarter     => "Last Quarter",
            MoonPhase::WaningCrescent  => "Waning Crescent",
        }
    }

    /// A single-glyph representation, matching this app's convention of a
    /// compact symbol string alongside every placement/aspect row.
    pub fn symbol(&self) -> &'static str {
        match self {
            MoonPhase::New            => "🌑",
            MoonPhase::WaxingCrescent  => "🌒",
            MoonPhase::FirstQuarter    => "🌓",
            MoonPhase::WaxingGibbous   => "🌔",
            MoonPhase::Full            => "🌕",
            MoonPhase::WaningGibbous   => "🌖",
            MoonPhase::LastQuarter     => "🌗",
            MoonPhase::WaningCrescent  => "🌘",
        }
    }
}

/// Moon phase from the Sun-Moon angular separation directly (Moon longitude
/// minus Sun longitude, 0-360°). Exposed separately from [`phase_at`] so
/// callers who already have both longitudes (e.g. a computed `Chart`) don't
/// need to recompute the ephemeris.
pub fn phase_from_longitudes(sun_lon: f64, moon_lon: f64) -> MoonPhase {
    let angle = (moon_lon - sun_lon).rem_euclid(360.0);
    // Each of the 8 phases covers a 45° segment, centered so that the exact
    // syzygy points (0°/90°/180°/270°) fall in the middle of New/First
    // Quarter/Full/Last Quarter rather than on a segment boundary.
    match angle {
        a if a < 22.5             => MoonPhase::New,
        a if a < 67.5             => MoonPhase::WaxingCrescent,
        a if a < 112.5            => MoonPhase::FirstQuarter,
        a if a < 157.5            => MoonPhase::WaxingGibbous,
        a if a < 202.5            => MoonPhase::Full,
        a if a < 247.5            => MoonPhase::WaningGibbous,
        a if a < 292.5            => MoonPhase::LastQuarter,
        a if a < 337.5            => MoonPhase::WaningCrescent,
        _                          => MoonPhase::New,
    }
}

/// Moon phase at a given Julian day number, computing Sun/Moon longitude
/// internally.
pub fn phase_at(jdn: f64) -> Result<MoonPhase, EphemerisError> {
    let positions = compute_positions(jdn)?;
    let sun = positions.get(Planet::Sun).ok_or(EphemerisError::Unavailable(Planet::Sun))?;
    let moon = positions.get(Planet::Moon).ok_or(EphemerisError::Unavailable(Planet::Moon))?;
    Ok(phase_from_longitudes(sun, moon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syzygy_points_land_on_the_right_named_phase() {
        assert_eq!(phase_from_longitudes(0.0, 0.0),   MoonPhase::New);
        assert_eq!(phase_from_longitudes(0.0, 90.0),  MoonPhase::FirstQuarter);
        assert_eq!(phase_from_longitudes(0.0, 180.0), MoonPhase::Full);
        assert_eq!(phase_from_longitudes(0.0, 270.0), MoonPhase::LastQuarter);
    }

    #[test]
    fn wraps_correctly_across_0_360() {
        // Moon just behind the Sun (angle ~359°) should read as New, not
        // Waning Crescent from a naive non-wrapping subtraction.
        assert_eq!(phase_from_longitudes(10.0, 5.0), MoonPhase::New);
        // Moon just ahead of the Sun by a few degrees is New too (both
        // sides of exact conjunction fall in the same 45° New segment).
        assert_eq!(phase_from_longitudes(350.0, 355.0), MoonPhase::New);
    }

    #[test]
    fn all_eight_phases_are_reachable_across_a_full_cycle() {
        // A fixed-length scan of circular phase space always revisits its
        // own starting segment at the far end (whatever degree you start
        // at, 360 steps later you're back there) — that's a property of
        // scanning a circle with a linear loop, not a bug in the (correctly
        // circular) phase boundaries. So check phase *reachability* (a set),
        // not an artificial "exactly N contiguous runs" count.
        use std::collections::HashSet;
        let seen: HashSet<MoonPhase> = (0..360)
            .map(|deg| phase_from_longitudes(0.0, deg as f64))
            .collect();
        assert_eq!(seen.len(), 8, "expected all 8 phases reachable, got {seen:?}");
    }

    #[test]
    fn full_moon_is_real_astronomy_not_just_this_apps_convention() {
        // A full moon is, by definition, Sun and Moon in opposition (180°
        // apart) as seen from Earth — this isn't an app-specific choice,
        // it's what "full moon" means.
        for offset in [178.0, 180.0, 182.0] {
            assert_eq!(phase_from_longitudes(100.0, (100.0 + offset) % 360.0), MoonPhase::Full);
        }
    }
}
