Feature: Authoring a legacy interpretation propagates to peers
  InterpOp::Author is the original whole-interpretation authoring model
  (superseded for new writes by DocOp::Edit's collaborative-doc model,
  but still the write path ZodiaClient::author uses, and still how
  pre-collaborative-doc content is represented on the wire). Like
  revoke, it is legacy-model, content-hash-addressed, and flows over
  the always-on global sync topic rather than a per-key one — no
  subscription to a specific key is needed to observe it. This is the
  one core write verb in ZodiaClient's public API that had no BDD
  coverage at all until now.

  Scenario: A connected peer observes another peer's authored interpretation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" authors an interpretation on "natal:venus_trine_jupiter"
    Then "Alice" observes the authored interpretation on "natal:venus_trine_jupiter" within 15 seconds
