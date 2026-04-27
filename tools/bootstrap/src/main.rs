//! Zodia bootstrap node.
//!
//! An always-on peer that anchors the global gossip swarm so clients have
//! somewhere to connect on first start.  Run this once on a public VPS, note
//! the printed `node_id`, then paste it into `net/src/network.rs`.
//!
//! Usage:
//!   cargo build --release -p zodia-bootstrap
//!   ./zodia-bootstrap [--key-file /etc/zodia/bootstrap.key] [--port 17434]

use std::fs;
use std::net::Ipv6Addr;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use futures_util::StreamExt;
use p2panda_core::PrivateKey as PandaKey;
use p2panda_net::iroh_endpoint::IrohConfig;
use p2panda_net::iroh_mdns::MdnsDiscoveryMode;
use p2panda_net::{AddressBook, Discovery, Endpoint, Gossip, MdnsDiscovery};
use zodia_core::topic_key_global;

/// Must match `NETWORK_ID` in `net/src/network.rs`.
const NETWORK_ID: [u8; 32] = *b"zodia-network-2024\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

/// Self-hosted relay — must stay in sync with net/src/network.rs.
pub const RELAY_ZODIA: &str = "https://stargaze.whatdoyouliketodo.com.";
pub const ALL_RELAYS:  &[&str] = &[RELAY_ZODIA];

const DEFAULT_PORT: u16 = 17434;

// ── args ──────────────────────────────────────────────────────────────────────

struct Args {
    key_file: PathBuf,
    port:     u16,
    /// Bind the IPv6 socket to loopback only.  Use on hosts without IPv6
    /// routing to suppress `sendmsg: Network is unreachable` noise from iroh's
    /// QUIC address discovery probes.
    no_ipv6:  bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        key_file: PathBuf::from("bootstrap.key"),
        port:     DEFAULT_PORT,
        no_ipv6:  false,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--key-file" => { i += 1; args.key_file = PathBuf::from(&raw[i]); }
            "--port"     => { i += 1; args.port = raw[i].parse().expect("port must be a number"); }
            "--no-ipv6"  => { args.no_ipv6 = true; }
            other        => { eprintln!("unknown arg: {other}"); std::process::exit(1); }
        }
        i += 1;
    }
    args
}

// ── identity ──────────────────────────────────────────────────────────────────

/// Load 32-byte seed from file, or generate and persist a fresh one.
fn load_or_create_key(path: &PathBuf) -> SigningKey {
    if path.exists() {
        let bytes = fs::read(path).expect("could not read key file");
        let bytes: [u8; 32] = bytes.try_into().expect("key file must be exactly 32 bytes");
        SigningKey::from_bytes(&bytes)
    } else {
        let key = SigningKey::generate(&mut rand_core::OsRng);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(path, key.as_bytes()).expect("could not write key file");
        tracing::info!(path = %path.display(), "generated new bootstrap identity");
        key
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZODIA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args();
    let signing_key = load_or_create_key(&args.key_file);
    let panda_key   = PandaKey::from_bytes(signing_key.as_bytes());
    let public_key  = panda_key.public_key();

    // Print the node_id so the operator can paste it into net/src/network.rs.
    println!();
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║           Zodia Bootstrap Node                          ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  node_id : {}  ║", hex::encode(public_key.as_bytes()));
    println!("║  port    : {:<47} ║", args.port);
    for url in ALL_RELAYS {
        println!("║  relay   : {:<47} ║", url);
    }
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Paste into net/src/network.rs:                         ║");
    println!("║  BOOTSTRAP_NODE_ID = Some({:?});", public_key.as_bytes());
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    let address_book = AddressBook::builder().spawn().await?;

    // On hosts without IPv6 routing, bind the v6 socket to loopback only so
    // iroh's QUIC address discovery probes don't flood logs with
    // "Network is unreachable" errors.  Pass --no-ipv6 to enable.
    let bind_ip_v6 = if args.no_ipv6 { Ipv6Addr::LOCALHOST } else { Ipv6Addr::UNSPECIFIED };
    let bind_port_v6 = if args.no_ipv6 { 0 } else { args.port };

    let mut ep_builder = Endpoint::builder(address_book.clone())
        .config(IrohConfig {
            bind_port_v4: args.port,
            bind_ip_v6,
            bind_port_v6,
            ..IrohConfig::default()
        })
        .network_id(NETWORK_ID)
        .private_key(panda_key);
    for url in ALL_RELAYS {
        ep_builder = ep_builder.relay_url(url.parse()?);
    }
    let endpoint = ep_builder.spawn().await?;

    // Passive mDNS — respond to queries but don't broadcast (public server).
    let _mdns = MdnsDiscovery::builder(address_book.clone(), endpoint.clone())
        .mode(MdnsDiscoveryMode::Passive)
        .spawn()
        .await?;

    let _discovery = Discovery::builder(address_book.clone(), endpoint.clone())
        .spawn()
        .await?;

    let gossip = Gossip::builder(address_book.clone(), endpoint.clone())
        .spawn()
        .await?;

    // Subscribe to the global Zodia topic — every client subscribes here too,
    // so joining this swarm is enough to bootstrap peer discovery.
    let topic  = topic_key_global();
    let handle = gossip.stream(topic.0).await?;
    let mut rx = handle.subscribe();

    tracing::info!("subscribed to global Zodia topic — ready for peers");

    let mut msg_total:  u64 = 0;
    let mut msg_period: u64 = 0;

    // Log a heartbeat every 5 minutes so operators can verify the node is
    // active and seeing traffic even when nothing else is logged.
    let mut stats_tick = tokio::time::interval(tokio::time::Duration::from_secs(300));
    stats_tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(msg)) => {
                    msg_total  += 1;
                    msg_period += 1;
                    tracing::debug!(bytes = msg.len(), "gossip message relayed");
                }
                Some(Err(e)) => tracing::warn!("gossip error: {e}"),
                None         => break,
            },
            _ = stats_tick.tick() => {
                tracing::info!(
                    total     = msg_total,
                    per_5min  = msg_period,
                    "bootstrap heartbeat",
                );
                msg_period = 0;
            }
        }
    }

    Ok(())
}
