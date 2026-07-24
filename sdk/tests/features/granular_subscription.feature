Feature: Granular per-key sync topics protect bandwidth and privacy
  Guarantees the core promise of docs/prd/granular-topic-subscription.md:
  a device that hasn't subscribed to a key does not receive live updates
  for it. This is the regression test that PRD's own progress notes
  flagged as missing ("Unsubscribed keys stay silent").

  Scenario: An unsubscribed peer does not receive another key's edit
    Given a peer named "Alice" connected to the network
    And a peer named "Bob" connected to the network
    And "Alice" is subscribed to "natal:sun_trine_moon"
    And "Bob" is subscribed to "natal:mars_square_saturn"
    When "Bob" edits "natal:mars_square_saturn"
    Then "Alice" observes no doc edit on "natal:mars_square_saturn" within 5 seconds
