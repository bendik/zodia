//! Simplified Keplerian ephemeris.
//!
//! Accuracy: ≤ 1–2° for most bodies over 1800–2200 CE, adequate for
//! astrological aspect detection (typical orbs are 3–8°).
//!
//! Production upgrade path: replace `compute_positions` with Swiss Ephemeris
//! FFI (`libswe`) via the `swisseph` crate for arcsecond precision.

use crate::planet::{Planet, PlanetPositions};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EphemerisError {
    #[error("position unavailable for {0:?}")]
    Unavailable(Planet),
}

/// Compute geocentric ecliptic longitudes for all bodies at the given JDN.
pub fn compute_positions(jdn: f64) -> Result<PlanetPositions, EphemerisError> {
    let d = jdn - 2451545.0; // days since J2000.0

    // Earth heliocentric true longitude and radius — used for all geocentric conversions
    let (l_e, r_e) = helio_true_lon_r(100.4664, 0.985_647, 102.9373, 1.000_000, 0.016_709, d);
    let l_e_rad = l_e.to_radians();

    let mut map = HashMap::new();

    // Sun: geocentric longitude = Earth heliocentric + 180°
    map.insert(Planet::Sun, (l_e + 180.0).rem_euclid(360.0));

    // Moon: simplified Brown/Meeus theory, accurate to ~1°
    map.insert(Planet::Moon, moon_longitude(d));

    // Outer and inner planets: heliocentric → geocentric via Earth subtraction
    //   l0 (°), n (°/day), ω (°), a (AU), e
    let planets = [
        (Planet::Mercury, 252.2504, 4.092_339, 77.4595,  0.387_098, 0.205_636),
        (Planet::Venus,   181.9798, 1.602_136, 131.5637, 0.723_332, 0.006_773),
        (Planet::Mars,    355.4333, 0.524_039, 336.0602, 1.523_679, 0.093_400),
        (Planet::Jupiter,  34.3515, 0.083_092,  14.3320, 5.202_561, 0.048_498),
        (Planet::Saturn,   50.0775, 0.033_460,  93.0574, 9.534_750, 0.055_546),
        (Planet::Uranus,  314.0550, 0.011_731, 173.0052, 19.18797,  0.046_381),
        (Planet::Neptune, 304.3487, 0.005_965,  48.1200, 30.06897,  0.009_456),
        (Planet::Pluto,   238.9290, 0.003_964, 224.0685, 39.48168,  0.248_808),
    ];

    for (planet, l0, n, omega, a, e) in planets {
        let (l_p, r_p) = helio_true_lon_r(l0, n, omega, a, e, d);
        let l_p_rad = l_p.to_radians();
        let x = r_p * l_p_rad.cos() - r_e * l_e_rad.cos();
        let y = r_p * l_p_rad.sin() - r_e * l_e_rad.sin();
        let geo = y.atan2(x).to_degrees().rem_euclid(360.0);
        map.insert(planet, geo);
    }

    Ok(PlanetPositions(map))
}

/// Heliocentric true longitude (degrees) and radius (AU) from Keplerian elements.
///
/// Elements at J2000.0: l0 = mean longitude, n = mean motion (°/day),
/// omega = longitude of perihelion, a = semi-major axis (AU), e = eccentricity.
fn helio_true_lon_r(l0: f64, n: f64, omega: f64, a: f64, e: f64, d: f64) -> (f64, f64) {
    let l = (l0 + n * d).rem_euclid(360.0);
    let m_deg = (l - omega).rem_euclid(360.0);
    let m = m_deg.to_radians();

    // Equation of center (radians) — two-term approximation
    let ec_rad = 2.0 * e * m.sin() + 1.25 * e * e * (2.0 * m).sin();
    let ec_deg = ec_rad.to_degrees();

    let true_lon = (l + ec_deg).rem_euclid(360.0);
    let nu = m + ec_rad; // true anomaly (radians, approx)
    let r = a * (1.0 - e * e) / (1.0 + e * nu.cos());

    (true_lon, r)
}

/// Moon geocentric ecliptic longitude, accurate to ~1° (simplified Meeus ch. 47).
fn moon_longitude(d: f64) -> f64 {
    let l = (218.316 + 13.176_396 * d).rem_euclid(360.0); // mean longitude
    let m = (134.963 + 13.064_993 * d).to_radians();       // mean anomaly
    let f = (93.272  + 13.229_350 * d).to_radians();       // argument of latitude
    (l + 6.289 * m.sin() - 1.274 * (m - 2.0 * f).sin() + 0.658 * (2.0 * f).sin())
        .rem_euclid(360.0)
}

/// Whether `planet` appears to be moving backward (retrograde) through the
/// zodiac at `jdn`, from Earth's point of view. Sun and Moon are never
/// retrograde in geocentric astrology — real bodies orbiting Earth-ward
/// don't loop the way outer/inner planets do from our vantage point — so
/// this always returns `false` for them without sampling.
///
/// Detected numerically: sign of the one-day-back longitude delta, handling
/// the 0°/360° wraparound. No analytic velocity term exists per-body in this
/// simplified model, and a numerical derivative is exact enough at orbital
/// timescales (a day is minutes of arc, not a full cycle, for every body
/// this app tracks).
pub fn is_retrograde(planet: crate::planet::Planet, jdn: f64) -> bool {
    use crate::planet::Planet;
    if matches!(planet, Planet::Sun | Planet::Moon) {
        return false;
    }
    let Ok(now) = compute_positions(jdn) else { return false };
    let Ok(yesterday) = compute_positions(jdn - 1.0) else { return false };
    let (Some(lon_now), Some(lon_prev)) = (now.get(planet), yesterday.get(planet)) else {
        return false;
    };
    let mut delta = lon_now - lon_prev;
    if delta > 180.0 { delta -= 360.0; }
    if delta < -180.0 { delta += 360.0; }
    delta < 0.0
}

#[cfg(test)]
mod retrograde_tests {
    use super::*;
    use crate::planet::Planet;

    /// Real invariant, not a model artifact: geocentric astrology never
    /// treats the Sun or Moon as retrograde, regardless of date.
    #[test]
    fn sun_and_moon_are_never_retrograde() {
        // Sample across a full year so this isn't true only by luck of one date.
        for i in 0..365 {
            let jdn = 2460676.5 + i as f64;
            assert!(!is_retrograde(Planet::Sun, jdn), "Sun retrograde at jdn {jdn}");
            assert!(!is_retrograde(Planet::Moon, jdn), "Moon retrograde at jdn {jdn}");
        }
    }

    /// Mercury retrogrades roughly 3-4 times a year for ~3 weeks each in
    /// reality — a well-known astrological fact independent of this app's
    /// simplified ephemeris precision. Assert the shape of that (goes
    /// retrograde AND direct within a year), not exact real-world dates,
    /// since this model isn't arcsecond-precise.
    #[test]
    fn mercury_goes_both_retrograde_and_direct_within_a_year() {
        let mut saw_retrograde = false;
        let mut saw_direct = false;
        for i in 0..365 {
            let jdn = 2460676.5 + i as f64;
            if is_retrograde(Planet::Mercury, jdn) { saw_retrograde = true; }
            else { saw_direct = true; }
        }
        assert!(saw_retrograde, "Mercury never appeared retrograde across a full year");
        assert!(saw_direct, "Mercury never appeared direct across a full year");
    }

    /// Outer planets spend meaningfully more of the year retrograde than
    /// Mercury does, since Earth "laps" them annually rather than them
    /// completing an inner orbit — a basic, well-known orbital-mechanics
    /// fact this numeric derivative should reproduce.
    #[test]
    fn outer_planets_retrograde_more_of_the_year_than_mercury() {
        let retro_days = |planet: Planet| -> u32 {
            (0..365).filter(|&i| is_retrograde(planet, 2460676.5 + i as f64)).count() as u32
        };
        let mercury_days = retro_days(Planet::Mercury);
        for &outer in &[Planet::Jupiter, Planet::Saturn, Planet::Uranus, Planet::Neptune, Planet::Pluto] {
            assert!(
                retro_days(outer) > mercury_days,
                "{outer:?} retrograde {} days, expected more than Mercury's {mercury_days}",
                retro_days(outer),
            );
        }
    }
}
