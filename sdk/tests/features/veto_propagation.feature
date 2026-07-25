Feature: Veto proposals propagate to peers watching the same key
  DocOp::Veto is the author-protection mechanism
  docs/prd/collaborative-interpretations.md promises: a ring author can
  propose reverting someone else's edit within a 7-day window. This
  guarantees the proposal itself reaches subscribed peers as
  StateEvent::DocVetoProposed — whether the veto is actually *honoured*
  (ring membership + window + newest-edit checks) is app-layer logic,
  not covered here (see docs/testing/coverage-and-bdd-scenarios.md §3).

  Scenario: A subscribed peer sees another peer's veto proposal
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:saturn_square_moon"
    And "Bob" is subscribed to "natal:saturn_square_moon"
    When "Bob" vetoes an edit on "natal:saturn_square_moon"
    Then "Alice" observes a veto proposal on "natal:saturn_square_moon" within 15 seconds
