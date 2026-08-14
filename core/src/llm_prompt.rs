//! The prompt sent to a local LLM (via `zodia-llm`) to draft an
//! interpretation. Deliberately our own house style, not a generic
//! "write me an astrology blurb" request — kept short and direct since
//! local models are often small and drift badly with long system prompts.

use crate::interp::{InterpKey, InterpKind};

/// Build the prompt for `key`. Pure and deterministic (no randomness, no
/// clock) so it's trivially unit-testable.
pub fn build_interp_prompt(key: &InterpKey) -> String {
    let subject = key.plain_name();
    let context = kind_context(key.kind());
    format!(
        "You are writing a short astrological interpretation for a natal-chart \
         reading app. Write about {subject} — {context}.\n\
         \n\
         Rules:\n\
         - 2 to 4 sentences, no more.\n\
         - Plain, warm, non-mystical language. Describe a psychological \
           tendency or dynamic, not a prediction or a fated event.\n\
         - No preamble (\"Here is...\"), no headings, no markdown formatting, \
           no disclaimers about not being a real astrologer.\n\
         - Output only the interpretation text itself.",
    )
}

fn kind_context(kind: InterpKind) -> &'static str {
    match kind {
        InterpKind::Natal =>
            "a natal chart aspect, a fixed part of someone's own personality",
        InterpKind::Synastry =>
            "a synastry aspect between two people's charts, describing how they interact with each other",
        InterpKind::Transit =>
            "a transiting planet currently activating a natal aspect",
        InterpKind::SkyAspect =>
            "an aspect between two planets in today's sky, not tied to any one person's chart",
        InterpKind::HouseTransit =>
            "a transiting planet currently moving through a natal house",
        InterpKind::PlacementSign =>
            "a planet's placement by zodiac sign in someone's natal chart",
        InterpKind::PlacementHouse =>
            "a planet's placement by house in someone's natal chart",
        InterpKind::PlacementAngle =>
            "a chart angle's (Ascendant/Midheaven) sign placement",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natal_prompt_names_the_aspect_and_frames_it_as_personality() {
        let key = InterpKey::Natal { aspect_sig: "moon_trine_venus".into() };
        let prompt = build_interp_prompt(&key);
        assert!(prompt.contains("Moon trine Venus"), "prompt: {prompt}");
        assert!(prompt.contains("own personality"), "prompt: {prompt}");
    }

    #[test]
    fn synastry_prompt_frames_it_as_a_relationship_between_two_people() {
        let key = InterpKey::Synastry { aspect_sig: "sun_square_mars".into() };
        let prompt = build_interp_prompt(&key);
        assert!(prompt.contains("Sun square Mars"), "prompt: {prompt}");
        assert!(prompt.contains("two people's charts"), "prompt: {prompt}");
    }

    #[test]
    fn every_kind_gets_a_distinct_context_sentence() {
        use std::collections::HashSet;
        let keys = [
            InterpKey::Natal { aspect_sig: "sun_trine_moon".into() },
            InterpKey::Synastry { aspect_sig: "sun_trine_moon".into() },
            InterpKey::Transit {
                transiting: crate::planet::Planet::Saturn,
                natal_body: crate::planet::Planet::Sun,
                kind: crate::aspects::AspectKind::Square,
            },
            InterpKey::SkyAspect { aspect_sig: "mars_conjunction_venus".into() },
            InterpKey::HouseTransit { transiting: crate::planet::Planet::Jupiter, house: 7 },
            InterpKey::PlacementSign { planet: crate::planet::Planet::Mercury, sign: 2 },
            InterpKey::PlacementHouse { planet: crate::planet::Planet::Mercury, house: 3 },
            InterpKey::PlacementAngle { angle: crate::interp::Angle::Ascendant, sign: 0 },
        ];
        let contexts: HashSet<&'static str> = keys.iter().map(|k| kind_context(k.kind())).collect();
        assert_eq!(contexts.len(), keys.len(), "two InterpKinds share a context sentence");
    }

    #[test]
    fn prompt_instructs_plain_output_with_no_preamble_or_markdown() {
        let key = InterpKey::Natal { aspect_sig: "sun_trine_moon".into() };
        let prompt = build_interp_prompt(&key);
        assert!(prompt.to_lowercase().contains("no preamble"));
        assert!(prompt.to_lowercase().contains("no markdown"));
    }
}
