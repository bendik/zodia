Feature: Explicitly unsubscribing from a key stops live updates on it
  ZodiaClient::unsubscribe closes a key's per-key sync topic immediately,
  as opposed to grace_period_unsubscribe.feature's auto-expiry after idle
  time. Both end up at "not subscribed", but only the grace-period path
  had BDD coverage before this — a bug that made unsubscribe() a no-op
  (topic left open) would have gone unnoticed since every other scenario
  either never subscribes or never unsubscribes at all.

  Scenario: A peer who unsubscribes from a key stops receiving its edits
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:mercury_retrograde_venus"
    And "Bob" is subscribed to "natal:mercury_retrograde_venus"
    When "Alice" unsubscribes from "natal:mercury_retrograde_venus"
    And "Bob" edits "natal:mercury_retrograde_venus"
    Then "Alice" observes no doc edit on "natal:mercury_retrograde_venus" within 5 seconds
