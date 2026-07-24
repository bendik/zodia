Feature: Collaborative interpretation editing converges across peers
  Guarantees the core promise of docs/prd/collaborative-interpretations.md
  at the data-flow layer: an edit published by one peer reaches every
  other peer subscribed to the same key. This does not cover whether the
  UI renders the edit, badges a notification, or lets the author veto it
  — those are app-layer behaviors tracked separately in
  docs/testing/coverage-and-bdd-scenarios.md.

  Scenario: A subscribed peer receives another peer's edit
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:venus_square_pluto"
    And "Bob" is subscribed to "natal:venus_square_pluto"
    When "Bob" edits "natal:venus_square_pluto"
    Then "Alice" observes a doc edit on "natal:venus_square_pluto" within 15 seconds
