Feature: Revoking an authored interpretation propagates to peers
  A user can retract their own previously authored content
  (InterpOp::Revoke) and have that retraction reach peers who already
  have it, materialising as StateEvent::InterpRevoked. This is the
  legacy authoring model (pre-collaborative-doc): content is addressed
  by a hash of (interp_key, body) rather than by key, and revocations
  flow over the single always-on global sync topic rather than a
  per-key one — unlike collaborative-doc operations, no subscription to
  any specific key is needed to observe a revocation.

  Scenario: A connected peer observes another peer's revocation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" revokes a contribution
    Then "Alice" observes the revocation within 15 seconds
