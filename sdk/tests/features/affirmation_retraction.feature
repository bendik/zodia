Feature: Retracting an affirmation propagates to peers
  A user can undo their own ♡ on a doc revision (DocOp::RetractAffirm),
  same self-service-undo shape as leaving a circle — a misclick shouldn't
  need anyone else's help to fix. Retraction targets the same
  (interp_key, target_rev) pair AffirmRev does; only the voter's own prior
  affirmation is removed, materialising as StateEvent::DocAffirmRetracted
  and deleting the (interp_key, target_rev, voter_pk) row from the local
  affirmations table wherever the retraction is received. Re-affirming
  after a retraction is not "sticky" — it's just another AffirmRev, so
  there's nothing new to prove there.

  Scenario: A subscribed peer sees another peer's affirmation retracted
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_trine_moon"
    And "Bob" is subscribed to "natal:sun_trine_moon"
    When "Bob" affirms the current revision of "natal:sun_trine_moon"
    Then "Alice" observes a doc affirmation on "natal:sun_trine_moon" within 15 seconds
    When "Bob" retracts their affirmation of the current revision of "natal:sun_trine_moon"
    Then "Alice" observes the doc affirmation retracted on "natal:sun_trine_moon" within 15 seconds

  Scenario: A retraction is correctly attributed to the retractor, not another affirmer
    # Bob and Carol both affirm the same revision; Bob then retracts.
    # Proven from Alice's side (a third, uninvolved peer) that the
    # retraction event is attributed to Bob specifically — not just "a
    # retraction happened", which would also pass if the materialiser
    # mixed up whose affirmation was removed.
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And a peer named "Carol" connected to the network
    And "Alice" is subscribed to "natal:venus_square_mars"
    And "Bob" is subscribed to "natal:venus_square_mars"
    And "Carol" is subscribed to "natal:venus_square_mars"
    When "Bob" affirms the current revision of "natal:venus_square_mars"
    And "Carol" affirms the current revision of "natal:venus_square_mars"
    And "Bob" retracts their affirmation of the current revision of "natal:venus_square_mars"
    Then "Alice" observes "Bob"'s affirmation retracted on "natal:venus_square_mars" within 15 seconds
