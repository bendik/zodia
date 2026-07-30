Feature: A peer's self-chosen display name propagates to other peers
  Truncated hex pubkeys are the only thing peers can identify each other by
  today. A peer can broadcast a self-chosen display name
  (InterpOp::SetDisplayName, legacy log 0, always-on global topic — same
  propagation model as Author/Revoke) so other peers can show something
  more human than "···4F438888". This is an untrusted hint, not an
  authoritative identity: any peer can claim any name, and each receiver
  keeps only the newest one they've seen per author.

  Scenario: A connected peer observes another peer's display name
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" sets their display name to "Bobby"
    Then "Alice" observes "Bob" set their display name to "Bobby" within 15 seconds

  Scenario: A later name change from the same peer supersedes the earlier one
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    When "Bob" sets their display name to "Bobby"
    And "Alice" observes "Bob" set their display name to "Bobby" within 15 seconds
    And "Bob" sets their display name to "Robert"
    Then "Alice" observes "Bob" set their display name to "Robert" within 15 seconds
