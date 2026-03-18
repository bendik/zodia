//! Zodia — a p2p astrological encounter network.

use zodia_core::{BirthData, Chart, InterpKind, Planet};
use zodia_net::SwarmTopics;

fn main() {
    // J2000.0 epoch: 2000-01-01 12:00 UT, London (5-char geohash)
    let birth = BirthData::new(2451545.0, "gcpvj");
    let chart = Chart::compute(birth.clone()).expect("ephemeris failed");

    println!("── natal chart ──────────────────────────────────────────");
    println!(
        "ASC {:.1}°  MC {:.1}°",
        chart.houses.ascendant,
        chart.houses.midheaven
    );
    println!(
        "Sun {:.1}°  Moon {:.1}°",
        chart.positions.get(Planet::Sun).unwrap_or(0.0),
        chart.positions.get(Planet::Moon).unwrap_or(0.0),
    );

    let natal = chart.natal_aspects();
    println!("{} natal aspects", natal.len());
    for a in &natal {
        println!("  {} {} {} (orb {:.1}°)", a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb);
    }

    // ── transits at a later date ─────────────────────────────────────────────
    // 2026-01-01 12:00 UT ≈ J2451545 + 9497 days
    let now_jdn = 2451545.0 + 9497.0;
    let transits = chart.transits_at(now_jdn).expect("transit computation failed");

    println!("\n── current transits ({} planet–natal aspects) ──────────", transits.transit_aspects.len());
    for ta in &transits.transit_aspects {
        println!(
            "  {} {} natal {} (orb {:.1}°)  key={}",
            ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb,
            ta.interp_key().to_sig(),
        );
    }

    println!("\n── house transits ───────────────────────────────────────");
    for ht in &transits.house_transits {
        println!("  {} in house {}  key={}", ht.transiting.symbol(), ht.house, ht.interp_key().to_sig());
    }

    println!("\n── interp keys by kind ──────────────────────────────────");
    let keys = transits.interp_keys();
    let transit_keys  = keys.iter().filter(|k| k.kind() == InterpKind::Transit).count();
    let house_keys    = keys.iter().filter(|k| k.kind() == InterpKind::HouseTransit).count();
    println!("  transit aspects: {transit_keys}  house transits: {house_keys}");

    let swarm = SwarmTopics::from_birth(&birth);
    println!("\nbroad topic:  {:x?}", &swarm.broad.as_bytes()[..4]);
    println!("narrow topic: {:x?}", &swarm.narrow.as_bytes()[..4]);
}
