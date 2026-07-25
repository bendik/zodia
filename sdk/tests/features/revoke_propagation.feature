Feature: Revoking an authored interpretation propagates to peers
  A user can retract their own content and have that retraction reach
  peers who already have it. InterpOp::Revoke and
  StateEvent::InterpRevoked already exist in the wire format and
  pipeline; ZodiaClient has no way to send one yet. Unlike DocOp,
  InterpOp::Revoke is legacy-model content-hash-addressed and flows over
  the always-on global topic, not a per-key one — no subscription step
  is needed for a peer to observe it.

  Scenario: A connected peer observes another peer's revocation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" revokes a contribution
    Then "Alice" observes the revocation within 15 seconds
