Feature: A peer subscribed to multiple keys receives each on the right key
  Each interp_key gets its own derived p2panda log_id and its own sync
  topic (zodia_ops::log_id_for_key, zodia_core::topic_key_for_interp).
  granular_subscription.feature proves a key you never subscribed to
  stays silent; this proves the complementary case — a peer subscribed
  to *several* keys at once still receives each edit tagged with the
  correct key, rather than events bleeding across keys because of a
  derivation collision or a routing bug in the multi-topic handle map.

  Found while writing this: opening two brand-new topics between the
  same peer pair back-to-back, then publishing on both immediately, is
  measurably flakier than a single-topic scenario — session
  establishment for the second topic has real, sometimes multi-second
  jitter (p2panda-net's per-topic LogSync session negotiation, not a
  zodia-sdk bug). A 1-second settle after subscribing still missed
  intermittently; 3 seconds plus a wider 25-second observation window
  was reliably clean across repeated runs. Real-world app usage doesn't
  usually open two new topics in the same instant, so this is noted
  here as a characteristic to be aware of, not fixed — see
  docs/testing/coverage-and-bdd-scenarios.md for the general pattern of
  documenting rather than hiding real-transport timing findings.

  Scenario: Edits to two different subscribed keys arrive correctly labelled
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_conjunction_mercury"
    And "Alice" is subscribed to "natal:moon_opposition_saturn"
    And "Bob" is subscribed to "natal:sun_conjunction_mercury"
    And "Bob" is subscribed to "natal:moon_opposition_saturn"
    And 3 seconds pass
    When "Bob" edits "natal:sun_conjunction_mercury"
    And "Bob" edits "natal:moon_opposition_saturn"
    Then "Alice" observes a doc edit on "natal:sun_conjunction_mercury" within 25 seconds
    And "Alice" observes a doc edit on "natal:moon_opposition_saturn" within 25 seconds
