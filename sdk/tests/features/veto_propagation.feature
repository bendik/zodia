Feature: Veto proposals propagate to peers watching the same key
  Each block of a collaborative doc remembers its recent authors (a
  "ring"). Any author in that ring can propose a veto — reverting a
  later edit — within a 7-day window after that edit landed. This
  proves only that the proposal itself (DocOp::Veto) reaches peers
  subscribed to the key, materialising as StateEvent::DocVetoProposed.
  Whether a given veto is actually *honoured* — checking the proposer
  is really in the ring, the 7-day window hasn't passed, and the target
  is still the newest edit — is app-layer logic that runs against local
  store state, not something an SDK-only scenario can exercise; that
  gap is tracked in docs/testing/coverage-and-bdd-scenarios.md.

  Scenario: A subscribed peer sees another peer's veto proposal
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:saturn_square_moon"
    And "Bob" is subscribed to "natal:saturn_square_moon"
    When "Bob" vetoes an edit on "natal:saturn_square_moon"
    Then "Alice" observes a veto proposal on "natal:saturn_square_moon" within 15 seconds
