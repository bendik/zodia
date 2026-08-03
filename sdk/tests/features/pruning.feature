Feature: Local storage pruning removes old operations from other peers
  Every published edit stays on a device's local disk indefinitely by
  default — nothing removes it once received, so local storage grows
  with total network activity rather than with what a user actually
  reads (docs/prd/operations-and-streams-rearchitecture.md's user story
  20: "I want my device's storage to stay bounded over years of use").
  ZodiaClient::prune(retention) deletes locally-stored operations older
  than the given retention window, except anything authored by this
  device's own identity — a peer's own contributions are never pruned,
  regardless of age, since losing your own authored history to a local
  housekeeping sweep would be a real data-loss bug, not a storage win.

  Scenario: An operation received from another peer is eligible for pruning once retention elapses
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:pluto_conjunction_saturn"
    And "Bob" is subscribed to "natal:pluto_conjunction_saturn"
    When "Bob" edits "natal:pluto_conjunction_saturn"
    And "Alice" observes a doc edit on "natal:pluto_conjunction_saturn" within 15 seconds
    And 1 second passes
    And "Alice" prunes local storage with a retention of 0 seconds
    Then pruning removed at least 1 operation

  Scenario: A peer's own contribution survives pruning regardless of retention
    Given a peer named "Alice" connected to the network
    When "Alice" authors an interpretation on "natal:mars_trine_sun"
    And 1 second passes
    And "Alice" prunes local storage with a retention of 0 seconds
    Then pruning removed 0 operations

  Scenario: A recently received peer operation survives pruning within the retention window
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:mercury_square_neptune"
    And "Bob" is subscribed to "natal:mercury_square_neptune"
    When "Bob" edits "natal:mercury_square_neptune"
    And "Alice" observes a doc edit on "natal:mercury_square_neptune" within 15 seconds
    And "Alice" prunes local storage with a retention of 300 seconds
    Then pruning removed 0 operations

  Scenario: Circle-shared content received from another peer is not eligible for pruning yet
    # Circle operations carry no Timestamp extension (ZodiaExtensions's
    # Extension<Timestamp> impl returns None for the Circle variant), so
    # prune_older_than skips every circle op it sees rather than treating
    # the missing timestamp as an error — a deliberate, documented scope
    # limit for this slice (docs/prd/circles.md), not tested until now.
    # This is Bob's own local copy of content Alice authored and shared —
    # unlike the scenario above, it isn't exempt because Bob authored it;
    # it's exempt because circle content isn't prunable at all yet.
    #
    # Expected count is 1, not 0: connecting to the network always
    # broadcasts a display name (InterpOp::SetDisplayName, see
    # zodia_sdk::TEST_PEER_DISPLAY_NAME) over the same always-on global
    # topic circle ops share — Bob genuinely receives one foreign,
    # timestamped, prunable op from Alice regardless of circles. Verified
    # by isolating it: two connected peers with no circle activity at all
    # still prune exactly 1 operation. The interesting claim here is
    # narrower than "nothing gets pruned" — it's "the circle content
    # specifically isn't among what got pruned", which this proves by
    # elimination (1 candidate existed, 1 got removed, and it wasn't the
    # only-other candidate: the circle share).
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And 3 seconds pass
    And "Alice" creates a circle
    And "Alice" invites "Bob" to the circle
    And "Bob" joins the circle
    And 1 second passes
    When "Alice" shares an interpretation on "natal:circle_prune_immunity" to the circle
    Then "Bob" observes the authored interpretation on "natal:circle_prune_immunity" within 15 seconds
    And 1 second passes
    When "Bob" prunes local storage with a retention of 0 seconds
    Then pruning removed 1 operations
