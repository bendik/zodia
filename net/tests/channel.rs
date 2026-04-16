//! Integration tests for `DirectChannel`.
//!
//! These tests create two real iroh QUIC endpoints on loopback (no relay,
//! no p2panda stack) and verify that `exchange_consent` correctly transfers
//! `ConsentBlob` values in both directions concurrently.

use iroh::{Endpoint, RelayMode};
use zodia_core::BirthData;
use zodia_net::{ConsentBlob, DirectChannel};
use zodia_net::network::CONSENT_PROTOCOL;

fn make_blob(seed: u8, jdn: f64, geohash: &str) -> ConsentBlob {
    ConsentBlob {
        birth:     BirthData::new(jdn, geohash),
        prekey:    [seed; 32],
        ephemeral: [seed.wrapping_add(1); 32],
        relay_pk:  [seed.wrapping_add(2); 32],
    }
}

/// Spin up two loopback iroh endpoints, connect them with the consent ALPN,
/// then run `exchange_consent` concurrently on both sides.  Each side must
/// receive the *other* side's blob unchanged.
#[tokio::test]
async fn exchange_consent_transfers_blobs_bidirectionally() {
    // ── build two loopback endpoints ─────────────────────────────────────────
    let server = Endpoint::builder()
        .alpns(vec![CONSENT_PROTOCOL.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("server bind failed");

    let client = Endpoint::builder()
        .alpns(vec![CONSENT_PROTOCOL.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("client bind failed");

    let server_addr = server.addr();

    // ── blobs to exchange ────────────────────────────────────────────────────
    let alice_blob = make_blob(0xAA, 2451545.0, "u4pruydqqvj"); // server side
    let bob_blob   = make_blob(0xBB, 2415021.0, "gbsvh");       // client side

    // ── server: accept one connection, run exchange_consent ──────────────────
    let alice_blob_c   = alice_blob.clone();
    let server_handle = tokio::spawn(async move {
        let conn = server
            .accept()
            .await
            .expect("no incoming connection")
            .await
            .expect("server handshake failed");
        let ch = DirectChannel::from_connection(conn);
        ch.exchange_consent(&alice_blob_c).await
    });

    // ── client: connect, run exchange_consent ────────────────────────────────
    let bob_blob_c = bob_blob.clone();
    let conn = client
        .connect(server_addr, CONSENT_PROTOCOL)
        .await
        .expect("client connect failed");
    let client_ch = DirectChannel::from_connection(conn);
    let received_by_client = client_ch
        .exchange_consent(&bob_blob_c)
        .await
        .expect("client exchange_consent failed");

    // ── collect server result ────────────────────────────────────────────────
    let received_by_server = server_handle
        .await
        .expect("server task panicked")
        .expect("server exchange_consent failed");

    // ── assertions ───────────────────────────────────────────────────────────
    // Client must have received Alice's (server's) blob.
    assert_eq!(received_by_client.birth.jdn,     alice_blob.birth.jdn);
    assert_eq!(received_by_client.birth.geohash, alice_blob.birth.geohash);
    assert_eq!(received_by_client.prekey,        alice_blob.prekey);
    assert_eq!(received_by_client.ephemeral,     alice_blob.ephemeral);
    assert_eq!(received_by_client.relay_pk,      alice_blob.relay_pk);

    // Server must have received Bob's (client's) blob.
    assert_eq!(received_by_server.birth.jdn,     bob_blob.birth.jdn);
    assert_eq!(received_by_server.birth.geohash, bob_blob.birth.geohash);
    assert_eq!(received_by_server.prekey,        bob_blob.prekey);
    assert_eq!(received_by_server.ephemeral,     bob_blob.ephemeral);
    assert_eq!(received_by_server.relay_pk,      bob_blob.relay_pk);
}

/// A second exchange over the same connection must also succeed —
/// verifying that the channel is reusable (each `exchange_consent` call
/// opens independent QUIC streams, so they don't interfere).
#[tokio::test]
async fn exchange_consent_is_reusable_on_same_channel() {
    let server = Endpoint::builder()
        .alpns(vec![CONSENT_PROTOCOL.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("server bind failed");

    let client = Endpoint::builder()
        .alpns(vec![CONSENT_PROTOCOL.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("client bind failed");

    let server_addr = server.addr();

    let server_blob = make_blob(0x01, 2451545.0, "u4pru");
    let client_blob = make_blob(0x02, 2451545.0, "gbsvh");

    let sb = server_blob.clone();
    let server_handle = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let ch = DirectChannel::from_connection(conn);
        // Two consecutive exchanges over the same channel.
        let r1 = ch.exchange_consent(&sb).await.unwrap();
        let r2 = ch.exchange_consent(&sb).await.unwrap();
        (r1, r2)
    });

    let conn = client.connect(server_addr, CONSENT_PROTOCOL).await.unwrap();
    let ch = DirectChannel::from_connection(conn);
    let cb = client_blob.clone();
    let r1 = ch.exchange_consent(&cb).await.unwrap();
    let r2 = ch.exchange_consent(&cb).await.unwrap();

    let (sr1, sr2) = server_handle.await.unwrap();

    // Both rounds should transfer correctly.
    assert_eq!(r1.prekey, server_blob.prekey);
    assert_eq!(r2.prekey, server_blob.prekey);
    assert_eq!(sr1.prekey, client_blob.prekey);
    assert_eq!(sr2.prekey, client_blob.prekey);
}
