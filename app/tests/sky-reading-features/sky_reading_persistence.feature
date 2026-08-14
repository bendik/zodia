Feature: A published Sky reading sticks — no stale "No reading yet" text
  The Sky feed's transit/sky-aspect/house-transit cards used to resolve
  their reading text from a static, bundled baseline file instead of the
  real community-reading database, so a published reading never showed up
  on the card — not immediately, and not after restarting with the same
  identity and data directory. This scenario proves both halves: the
  in-session refresh (transit_ticker.rs + FeedView::push updating a card in
  place) and the after-restart read (transit_ticker.rs reading through the
  store instead of only the static baseline).

  Scenario: A published Sky reading is visible immediately and after a restart
    Given a lone instance "Astrid" born in Oslo
    When she publishes a reading for the first active sky transit
    Then the Sky feed shows the reading immediately
    When the instance restarts using the same data directory
    Then the Sky feed still shows the reading
