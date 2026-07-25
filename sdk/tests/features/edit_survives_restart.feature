Feature: A published edit survives its author's client restarting
  Every publish commits to local SQLite storage before any network
  broadcast is attempted (see sync/src/lib.rs::publish_bytes), so an
  edit is durable the moment the publish call returns, independent of
  whether any peer received it live. LogSync is peer-to-peer, not a
  persistent server, so a peer only serves what it has while it is
  actively subscribed to that key's topic — this scenario has Alice
  re-subscribe after reconnecting (the realistic "app restarted, reopens
  the page it last had open" flow) so Bob has someone to catch up from.

  Scenario: An edit survives its author's client restarting
    Given a peer named "Alice" connected to the network
    And "Alice" is subscribed to "natal:mercury_trine_jupiter"
    When "Alice" edits "natal:mercury_trine_jupiter"
    And "Alice" disconnects
    And "Alice" reconnects using the same identity and data directory
    And "Alice" is subscribed to "natal:mercury_trine_jupiter"
    And a peer named "Bob" connected to the network
    And "Bob" is subscribed to "natal:mercury_trine_jupiter"
    Then "Bob" observes a doc edit on "natal:mercury_trine_jupiter" within 15 seconds
