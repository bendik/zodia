Feature: An edit reaches every subscribed peer, not just one
  Every earlier collaborative-editing scenario in this suite proves
  convergence between exactly two peers. A key's community body is
  meant to converge across everyone interested in it, not just a pair —
  this proves the same edit reaches two independent subscribers at
  once, which a strictly pairwise test could pass by coincidence (e.g.
  if delivery were accidentally scoped to "the first peer who asked"
  rather than "every subscriber").

  Scenario: Two independent subscribers both receive the same edit
    Given a peer named "Alice" connected to the network
    And a peer named "Carol" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:neptune_trine_sun"
    And "Carol" is subscribed to "natal:neptune_trine_sun"
    And "Bob" is subscribed to "natal:neptune_trine_sun"
    When "Bob" edits "natal:neptune_trine_sun"
    Then "Alice" observes a doc edit on "natal:neptune_trine_sun" within 15 seconds
    And "Carol" observes a doc edit on "natal:neptune_trine_sun" within 15 seconds
