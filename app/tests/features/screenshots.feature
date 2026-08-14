Feature: Release screenshots are generated from a live two-peer session
  The four Flathub/website listing screenshots (chart, network, synastry,
  circles — see app/data/io.github.bendik.Zodia.metainfo.xml) are not
  mockups: they're captured from two real app instances discovering each
  other over the actual P2P stack, exchanging Tier-1 consent, and sharing
  a circle. Each instance drives itself through its own message layer
  (ZODIA_SCREENSHOT_SCRIPT — see app/src/screenshot_script.rs), not
  through synthetic mouse input, so a UI redesign changes the pictures
  but never breaks the pipeline.

  This scenario needs a display (any Wayland/X11 session, or a headless
  compositor such as `cage`) and real UDP networking for mDNS discovery,
  so it only runs when ZODIA_SCREENSHOTS_OUT is set — `cargo test` stays
  green everywhere else. Run it via scripts/generate-screenshots.sh.

  Scenario: The four listing screenshots come from one live session
    Given a primary instance "Astrid" born in Oslo
    And a secondary instance "Luna" born in Bergen
    When both instances run their screenshot scripts to completion
    Then a fresh chart screenshot exists
    And a fresh network screenshot exists
    And a fresh synastry screenshot exists
    And a fresh circles screenshot exists
