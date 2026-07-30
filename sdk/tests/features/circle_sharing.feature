Feature: Private circles let a user share an interpretation with named friends instead of the whole network
  A user can create a circle, invite specific peers into it by their
  network identity, and share an interpretation that only current
  circle members can decrypt and read (p2panda-spaces group encryption
  under the hood, see docs/prd/circles.md). Circle membership requires
  each peer to have already discovered the other's encryption key
  bundle — this happens automatically once both peers have been online
  together, via a well-known directory topic every device subscribes
  to on connect, so a short settle window after connecting is needed
  before inviting. Inviting someone only updates the inviter's own
  side; the invitee separately has to join the circle's topic (standing
  in for them learning the circle's id via some real invite notification
  or link, out of scope here) before they receive anything. A peer who
  was never invited receives nothing readable, even if they join the
  same topic — the ciphertext reaches every subscriber, but only an
  invited member can derive the group secret to decrypt it.

  Scenario: An invited circle member reads a privately shared interpretation
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And 1 second passes
    When "Alice" shares an interpretation on "natal:circle_shared_reading" to the circle
    Then "Bob" observes the authored interpretation on "natal:circle_shared_reading" within 15 seconds

  Scenario: A peer who was never invited does not receive a circle share
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And a peer named "Carol" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And "Carol" joins the circle
    And 1 second passes
    When "Alice" shares an interpretation on "natal:circle_excluded_reading" to the circle
    Then "Bob" observes the authored interpretation on "natal:circle_excluded_reading" within 15 seconds
    And "Carol" observes no authored interpretation on "natal:circle_excluded_reading" within 5 seconds
