Feature: Grace-period unsubscribe for non-chart aspect pages
  Subscribing to a key costs an open sync topic for as long as the
  subscription lives. A key outside the user's own natal chart should
  subscribe on demand (e.g. while its page is open) and unsubscribe
  again once nothing is actively using it, rather than accumulating
  permanent subscriptions for every key ever browsed.
  `ZodiaClient::touch_subscription(key, grace)` implements this:
  subscribing (idempotently) and starting a countdown that
  auto-unsubscribes unless touched again before it elapses. Grace
  periods below are seconds, for test speed; production wiring uses a
  longer real-world default.

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
