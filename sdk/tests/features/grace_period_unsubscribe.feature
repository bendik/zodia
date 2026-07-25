Feature: Grace-period unsubscribe for non-chart aspect pages
  Completes docs/prd/granular-topic-subscription.md's remaining scope: a
  key browsed outside the user's own chart subscribes on demand and
  unsubscribes again after being idle, instead of accumulating
  subscriptions forever. Short grace periods below are for test speed;
  production wiring uses a longer real-world default.

  Scenario: An idle subscription unsubscribes after its grace period
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" touches subscription to "natal:jupiter_opposition_uranus" with a grace period of 1 second
    When 2 seconds pass
    And "Bob" edits "natal:jupiter_opposition_uranus"
    Then "Alice" observes no doc edit on "natal:jupiter_opposition_uranus" within 5 seconds

  Scenario: Re-touching a subscription keeps it alive past the grace period
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" touches subscription to "natal:jupiter_opposition_uranus" with a grace period of 2 seconds
    When 1 second passes
    And "Alice" touches subscription to "natal:jupiter_opposition_uranus" with a grace period of 2 seconds
    And 1 second passes
    And "Bob" edits "natal:jupiter_opposition_uranus"
    Then "Alice" observes a doc edit on "natal:jupiter_opposition_uranus" within 5 seconds
