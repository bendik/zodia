Feature: A slow event subscriber sees Lagged rather than stalling the publisher
  zodia-sdk's StateEvent stream is a bounded broadcast channel (capacity
  256, see docs/testing/coverage-and-bdd-scenarios.md's Priority Gap #6).
  If a subscriber falls far enough behind, tokio's broadcast channel
  drops the oldest unread messages and returns Lagged(n) on the
  subscriber's next recv() rather than either blocking the publisher or
  growing without bound — a slow reader loses its own history, but
  never slows down the publisher or any other subscriber.

  Scenario: A subscriber who never reads falls behind and is told so
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" authors 300 interpretations rapidly
    And 5 seconds pass
    Then "Alice" observes a lagged events error within 5 seconds
