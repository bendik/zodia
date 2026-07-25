Feature: Editor presence heartbeats propagate to peers watching the same key
  DocOp::EditorPresence is a lightweight "I am editing this key right
  now" heartbeat (joined = true) or "I've stopped" (joined = false) —
  not stored long-term, just relayed live so peers on the same key can
  show "someone is editing this" indicators. This proves the heartbeat
  itself reaches subscribed peers as StateEvent::EditorPresenceChanged
  with the correct joined value; whether the UI actually renders a
  presence indicator from it is app-layer, not covered here.

  Scenario: A subscribed peer observes another peer starting to edit
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:pluto_sextile_venus"
    And "Bob" is subscribed to "natal:pluto_sextile_venus"
    When "Bob" starts editing "natal:pluto_sextile_venus"
    Then "Alice" observes editor presence joined on "natal:pluto_sextile_venus" within 15 seconds

  Scenario: A subscribed peer observes another peer stopping editing
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:pluto_sextile_venus"
    And "Bob" is subscribed to "natal:pluto_sextile_venus"
    When "Bob" stops editing "natal:pluto_sextile_venus"
    Then "Alice" observes editor presence left on "natal:pluto_sextile_venus" within 15 seconds
