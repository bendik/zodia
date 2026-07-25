Feature: Affirmations propagate to peers watching the same key
  DocOp::AffirmRev affirms a specific revision of a key's collaborative
  doc (interp_key + a 32-byte revision hash), not a whole competing
  interpretation the way the old InterpOp::Affirm model did. The
  community ranking for a key is "how many distinct peers have affirmed
  its current revision" — that only converges if the affirmation
  actually reaches every peer watching that key. This proves the
  data-layer half of that: whether the UI's displayed count updates
  correctly afterward is a separate, app-layer concern.

  Scenario: A subscribed peer sees another peer's affirmation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_trine_moon"
    And "Bob" is subscribed to "natal:sun_trine_moon"
    When "Bob" affirms the current revision of "natal:sun_trine_moon"
    Then "Alice" observes a doc affirmation on "natal:sun_trine_moon" within 15 seconds
