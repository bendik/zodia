Feature: Granular per-key sync topics protect bandwidth and privacy
  Each interp_key has its own sync topic and its own p2panda log, derived
  deterministically from the key. A device only receives live updates
  for a key once it has explicitly subscribed to that key's topic —
  being connected to the network at all is not enough. This is the
  regression test for that exact promise: a peer subscribed to one key
  must not receive live edits published to a different key it never
  subscribed to.

  Scenario: An unsubscribed peer does not receive another key's edit
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_trine_moon"
    And "Bob" is subscribed to "natal:mars_square_saturn"
    When "Bob" edits "natal:mars_square_saturn"
    Then "Alice" observes no doc edit on "natal:mars_square_saturn" within 5 seconds
