Feature: Affirmations propagate to peers watching the same key
  Guarantees AffirmRev ops — the signal collaborative-interpretations.md
  uses in place of the old per-row affirm model — reach subscribed peers.
  This is the data-layer half of "the community ranking converges across
  peers"; whether the UI's affirmation count updates correctly is a
  separate, app-layer concern.

  Scenario: A subscribed peer sees another peer's affirmation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_trine_moon"
    And "Bob" is subscribed to "natal:sun_trine_moon"
    When "Bob" affirms the current revision of "natal:sun_trine_moon"
    Then "Alice" observes a doc affirmation on "natal:sun_trine_moon" within 15 seconds
