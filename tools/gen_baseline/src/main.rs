//! Generate `baseline_aspects.toml` by prompting a local Ollama model for every
//! possible interpretation key in the Zodia schema.
//!
//! Usage:
//!   cargo run -p gen_baseline -- [OPTIONS]
//!
//! Options:
//!   --model NAME      Ollama model name (default: llama3.2)
//!   --host HOST:PORT  Ollama host (default: localhost:11434)
//!   --output FILE     Output path (default: baseline_aspects.toml)
//!   --resume          Skip keys that already exist in the output file

use std::{
    collections::HashMap,
    path::PathBuf,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use zodia_core::{AspectKind, Planet, sign_name_lower};

// ── all aspect kinds ──────────────────────────────────────────────────────────

const ALL_ASPECTS: &[AspectKind] = &[
    AspectKind::Conjunction,
    AspectKind::SemiSextile,
    AspectKind::Sextile,
    AspectKind::Square,
    AspectKind::Trine,
    AspectKind::Quincunx,
    AspectKind::Opposition,
];

// ── TOML shape mirrors BaselineData ──────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Baseline {
    #[serde(default)]
    natal:         HashMap<String, String>,
    #[serde(default)]
    synastry:      HashMap<String, String>,
    #[serde(default)]
    transit:       HashMap<String, String>,
    #[serde(default)]
    house_transit: HashMap<String, String>,
    /// Sign + house together — `InterpKey::Placement`'s merged key, one
    /// reading per planet×sign×house combination.
    #[serde(default)]
    placement:     HashMap<String, String>,
}

// ── key enumeration ───────────────────────────────────────────────────────────

/// Returns `(section, key_within_section, plain_name, context_hint)` for every
/// key the baseline TOML needs to cover.
fn all_keys() -> Vec<(&'static str, String, String, &'static str)> {
    let planets = Planet::all();
    let mut keys = Vec::new();

    // Natal: unordered pairs of planets × every aspect kind
    for i in 0..planets.len() {
        for j in (i + 1)..planets.len() {
            for &asp in ALL_ASPECTS {
                let sig = sorted_sig(planets[i], asp, planets[j]);
                let name = plain_natal(&sig);
                keys.push(("natal", sig, name, "natal chart aspect — a recurring dynamic in this person's character"));
            }
        }
    }

    // Synastry: same unordered pairs, but the dynamic is between two people
    for i in 0..planets.len() {
        for j in (i + 1)..planets.len() {
            for &asp in ALL_ASPECTS {
                let sig = sorted_sig(planets[i], asp, planets[j]);
                let name = plain_natal(&sig);
                keys.push(("synastry", sig, name, "synastry aspect — the dynamic that arises between these two charts"));
            }
        }
    }

    // Transit: ordered (transiting, natal) pairs × every aspect kind
    for &tr in planets {
        for &nat in planets {
            for &asp in ALL_ASPECTS {
                let sig = format!("{}_{}_{}", tr.name(), asp.name(), nat.name());
                let name = format!(
                    "{} {} {} (transit)",
                    cap(tr.name()), asp.display_name(), cap(nat.name())
                );
                keys.push(("transit", sig, name, "current transit — a temporary influence moving through the chart"));
            }
        }
    }

    // House transits: every planet × every house
    for &planet in planets {
        for house in 1u8..=12 {
            let sig = format!("{}:{}", planet.name(), house);
            let name = format!("{} in house {}", cap(planet.name()), house);
            keys.push(("house_transit", sig, name, "planet transiting a natal house — the area of life being activated"));
        }
    }

    // Placements: every planet × every sign × every house — merged key
    // (`InterpKey::Placement`), sign and house read together as one
    // reading rather than two. 10 planets × 12 signs × 12 houses = 1440
    // entries, a real jump from the old per-axis 240 (120 sign + 120
    // house) — this is the accepted tradeoff of merging the two keys:
    // full coverage now needs the full cross product, not just each axis.
    for &planet in planets {
        for sign in 0u8..12 {
            for house in 1u8..=12 {
                let sig = format!("{}:{}:{house}", planet.name(), sign_name_lower(sign));
                let name = format!(
                    "{} in {}, house {house}", cap(planet.name()), cap(sign_name_lower(sign)),
                );
                keys.push(("placement", sig, name,
                    "natal placement — a planet's sign and house together, the style and the arena"));
            }
        }
    }

    keys
}

fn sorted_sig(a: Planet, kind: AspectKind, b: Planet) -> String {
    let mut names = [a.name(), b.name()];
    names.sort_unstable();
    format!("{}_{}_{}", names[0], kind.name(), names[1])
}

/// Reconstruct a plain name from a sorted natal sig like "moon_trine_venus".
fn plain_natal(sig: &str) -> String {
    for &asp in ALL_ASPECTS {
        let needle = format!("_{}_", asp.name());
        if let Some(pos) = sig.find(&needle) {
            let a = cap(&sig[..pos]);
            let b = cap(&sig[pos + needle.len()..]);
            return format!("{} {} {}", a, asp.display_name(), b);
        }
    }
    sig.split('_').map(|w| cap(w)).collect::<Vec<_>>().join(" ")
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── Ollama call ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OllamaResp {
    response: String,
}

const STYLE_EXAMPLES: &str = r#"Examples of the style:
"Moon trine Venus" → Warmth is given easily. Home and beauty feel like the same thing. Comfort is offered before it's asked for.
"Saturn square Sun (transit)" → What doesn't hold is being shown. The scaffolding is stress-tested. Frustration is information.
"Jupiter in house 7" → Relationships become expansive. What seemed fixed opens. The other person is a mirror for possibility."#;

fn prompt_ollama(
    host: &str,
    model: &str,
    plain_name: &str,
    context: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let prompt = format!(
        "You write short astrological interpretations.\n\
         Each is 2–4 sentences. Present tense. No second person (no \"you\" or \"your\").\n\
         Abstract and observational. Minimal jargon. Plain English, slightly poetic.\n\n\
         {STYLE_EXAMPLES}\n\n\
         Placement: {plain_name}\n\
         Type: {context}\n\n\
         Reply with ONLY the interpretation text. No quotes, no labels, no explanation."
    );

    let resp: OllamaResp = ureq::post(&format!("http://{host}/api/generate"))
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({
            "model": model,
            "prompt": prompt,
            "stream": false,
        }))?
        .into_json()?;

    let text = resp.response.trim().to_string();
    Ok(text)
}

// ── args ──────────────────────────────────────────────────────────────────────

struct Args {
    model:  String,
    host:   String,
    output: PathBuf,
    resume: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        model:  "llama3.2".into(),
        host:   "localhost:11434".into(),
        output: PathBuf::from("baseline_aspects.toml"),
        resume: false,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--model"  => { i += 1; args.model  = raw[i].clone(); }
            "--host"   => { i += 1; args.host   = raw[i].clone(); }
            "--output" => { i += 1; args.output = PathBuf::from(&raw[i]); }
            "--resume" => { args.resume = true; }
            other      => { eprintln!("unknown arg: {other}"); std::process::exit(1); }
        }
        i += 1;
    }
    args
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();

    // Verify Ollama is reachable before starting the long run.
    if let Err(e) = ureq::get(&format!("http://{}/api/tags", args.host)).call() {
        eprintln!("error: cannot reach Ollama at http://{} — {e}", args.host);
        eprintln!("       Start Ollama and make sure the model is pulled:");
        eprintln!("       ollama pull {}", args.model);
        std::process::exit(1);
    }

    // Load existing output for --resume.
    let mut baseline: Baseline = if args.resume && args.output.exists() {
        let src = std::fs::read_to_string(&args.output).unwrap_or_default();
        toml::from_str(&src).unwrap_or_default()
    } else {
        Baseline::default()
    };

    let keys = all_keys();
    let total = keys.len();
    let mut done = 0usize;
    let mut skipped = 0usize;
    let start = Instant::now();

    eprintln!("generating {total} interpretations with model '{}'...", args.model);

    for (section, key, plain_name, context) in &keys {
        let already_has = match *section {
            "natal"         => baseline.natal        .contains_key(key.as_str()),
            "synastry"      => baseline.synastry     .contains_key(key.as_str()),
            "transit"       => baseline.transit      .contains_key(key.as_str()),
            "house_transit" => baseline.house_transit.contains_key(key.as_str()),
            "placement"     => baseline.placement    .contains_key(key.as_str()),
            _               => false,
        };
        if already_has {
            skipped += 1;
            continue;
        }

        done += 1;
        let remaining = total - done - skipped;
        let elapsed   = start.elapsed().as_secs();
        let rate = if done > 0 { elapsed as f64 / done as f64 } else { 0.0 };
        let eta_s = (remaining as f64 * rate) as u64;
        eprintln!(
            "[{done}/{total}] {plain_name}  (eta ~{}m{}s)",
            eta_s / 60, eta_s % 60
        );

        let mut text = None;
        for attempt in 1..=3 {
            match prompt_ollama(&args.host, &args.model, plain_name, context) {
                Ok(t) if t.len() >= 20 => { text = Some(t); break; }
                Ok(t) => eprintln!("  attempt {attempt}: too short ({} chars), retrying", t.len()),
                Err(e) => eprintln!("  attempt {attempt}: error — {e}"),
            }
        }

        let Some(t) = text else {
            eprintln!("  skipping {key} after 3 failed attempts");
            continue;
        };

        match *section {
            "natal"         => { baseline.natal        .insert(key.clone(), t); }
            "synastry"      => { baseline.synastry     .insert(key.clone(), t); }
            "transit"       => { baseline.transit      .insert(key.clone(), t); }
            "house_transit" => { baseline.house_transit.insert(key.clone(), t); }
            "placement"     => { baseline.placement    .insert(key.clone(), t); }
            _ => {}
        }

        // Write after every successful entry so a crash loses at most one key.
        if let Ok(toml) = toml::to_string_pretty(&baseline) {
            let _ = std::fs::write(&args.output, &toml);
        }
    }

    eprintln!(
        "done. {done} generated, {skipped} skipped. output: {}",
        args.output.display()
    );
}
