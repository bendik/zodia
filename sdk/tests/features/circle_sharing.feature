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

  Scenario: A member invited with write access can also share into the circle
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And a peer named "Carol" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle with write access
    And "Alice" invites "Carol" to the circle
    And "Bob" joins the circle
    And "Carol" joins the circle
    And 1 second passes
    When "Bob" shares an interpretation on "natal:circle_member_write_share" to the circle
    Then "Carol" observes the authored interpretation on "natal:circle_member_write_share" within 15 seconds

  Scenario: circle_members reflects invites and revocations accurately
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle with write access
    Then "Alice" sees "Bob" as a write member of the circle
    When "Alice" revokes "Bob" from the circle
    Then "Alice" no longer sees "Bob" as a member of the circle

  Scenario: A revoked circle member no longer receives new shares
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And 1 second passes
    When "Alice" shares an interpretation on "natal:circle_revoke_before" to the circle
    Then "Bob" observes the authored interpretation on "natal:circle_revoke_before" within 15 seconds
    When "Alice" revokes "Bob" from the circle
    And "Alice" shares an interpretation on "natal:circle_revoke_after" to the circle
    Then "Bob" observes no authored interpretation on "natal:circle_revoke_after" within 5 seconds

  Scenario: Revoking then reinviting a member restores their access, with a new level
    # p2panda-auth has no in-place access-level promotion (AlreadyAdded on
    # an active member) — revoke-then-reinvite is the documented workaround
    # for "give this read-only member write access instead". Nobody had
    # proven the workaround actually works end-to-end until this scenario.
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And 1 second passes
    When "Alice" revokes "Bob" from the circle
    And "Alice" invites "Bob" to the circle with write access
    And "Bob" shares an interpretation on "natal:circle_reinvite_write_share" to the circle
    Then "Alice" observes the authored interpretation on "natal:circle_reinvite_write_share" within 15 seconds

  Scenario: A member of one circle does not see another circle's shares
    # Both circles are authored by the same peer (Alice), which matters
    # here specifically: all of one author's circles currently share one
    # underlying log (zodia_circles::CIRCLE_LOG_ID is fixed), so Carol —
    # a member of Beta but never invited to Alpha — still receives
    # Alpha's ciphertext over the wire. This proves she genuinely can't
    # decrypt it, not just that she wasn't sent it.
    #
    # Two invites into two different circles for overlapping peer pairs
    # back-to-back is the same class of p2panda-net multi-topic session-
    # negotiation jitter documented for multi_key_isolation.feature.
    # Unlike that case, extra settle time here did not reduce the failure
    # rate (tried 3s, then 5s+1s+3s split across circle creation) — the
    # invite's own internal 25s retry loop is what actually matters, and
    # this specific combination (3 peers, 2 circles from one author,
    # sharing one underlying log per CIRCLE_LOG_ID) still occasionally
    # exceeds it. Documented as an accepted flaky characteristic of this
    # scenario, same as multi_key_isolation.feature, rather than chased
    # further — real app usage doesn't create two circles back-to-back
    # the instant two peers connect the way this deliberately stresses.
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And a peer named "Carol" connected to the network
    And 5 seconds pass
    And "Alice" creates a circle named "Alpha"
    And 1 second passes
    And "Alice" creates a circle named "Beta"
    And 3 seconds pass
    And "Alice" invites "Bob" to circle "Alpha"
    And "Alice" invites "Carol" to circle "Beta"
    And "Bob" joins circle "Alpha"
    And "Carol" joins circle "Beta"
    And 1 second passes
    When "Alice" shares an interpretation on "natal:circle_isolation_alpha" to circle "Alpha"
    Then "Bob" observes the authored interpretation on "natal:circle_isolation_alpha" within 15 seconds
    And "Carol" observes no authored interpretation on "natal:circle_isolation_alpha" within 5 seconds

  Scenario: A revoked member's own view of the circle converges too
    # Every other revocation scenario only checks state from the circle
    # owner's side (Alice). This checks it from Bob's own client instead —
    # does the revoke message actually reach and get applied by the
    # person it targets, or does only the revoker's own state update?
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And 1 second passes
    When "Alice" revokes "Bob" from the circle
    And 3 seconds pass
    Then "Bob" no longer sees themself as a member of the circle
