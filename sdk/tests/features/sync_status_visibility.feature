Feature: Sync status reflects real catch-up progress with peers
  ZodiaClient::sync_status() exposes a live snapshot (peers_known,
  peers_caught_up) derived from SyncStarted/SyncFinished/Failed
  lifecycle events, meant to back a UI panel like "caught up with 2 of
  3 peers" — this exists in the SDK but had no test proving it actually
  reflects a real catch-up, as opposed to staying at its zero-value
  default forever.

  Scenario: Connecting to a peer moves sync_status off its default
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:jupiter_trine_moon"
    And "Bob" is subscribed to "natal:jupiter_trine_moon"
    When "Bob" edits "natal:jupiter_trine_moon"
    Then "Alice" observes a doc edit on "natal:jupiter_trine_moon" within 15 seconds
    And "Alice" has caught up with at least 1 peer

  Scenario: sync_status aggregates catch-up across more than one peer
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And a peer named "Carol" connected to the network
    And "Alice" is subscribed to "natal:venus_conjunction_mars"
    And "Bob" is subscribed to "natal:venus_conjunction_mars"
    And "Alice" is subscribed to "natal:mars_conjunction_venus"
    And "Carol" is subscribed to "natal:mars_conjunction_venus"
    And 3 seconds pass
    When "Bob" edits "natal:venus_conjunction_mars"
    And "Carol" edits "natal:mars_conjunction_venus"
    Then "Alice" observes a doc edit on "natal:venus_conjunction_mars" within 15 seconds
    And "Alice" observes a doc edit on "natal:mars_conjunction_venus" within 15 seconds
    And "Alice" has caught up with at least 2 peers
