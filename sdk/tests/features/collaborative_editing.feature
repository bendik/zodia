Feature: Collaborative interpretation editing converges across peers
  Each aspect key (e.g. "natal:venus_square_pluto") has one shared,
  collaboratively-edited text instead of a list of separate
  interpretations competing for attention. That only holds together if
  an edit one peer makes actually reaches every other peer currently
  interested in that key. This proves that data-flow guarantee only —
  an edit published by one peer reaches every peer subscribed to the
  same key. It does not cover whether the UI renders the edit, badges a
  notification, or lets the author veto it; those are app-layer
  behaviors with their own gap tracked in
  docs/testing/coverage-and-bdd-scenarios.md.

  Scenario: A subscribed peer receives another peer's edit
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:venus_square_pluto"
    And "Bob" is subscribed to "natal:venus_square_pluto"
    When "Bob" edits "natal:venus_square_pluto"
    Then "Alice" observes a doc edit on "natal:venus_square_pluto" within 15 seconds
