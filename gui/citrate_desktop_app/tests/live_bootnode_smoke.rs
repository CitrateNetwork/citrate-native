//! NATIVE-R1-S2 WP-A2 / C3: live bootnode smoke.
//!
//! Gated (`#[ignore]`) — talks to the real testnet-beta bootnodes, so it
//! never runs in CI's default lane. Run it explicitly:
//!
//! ```sh
//! cargo test --test live_bootnode_smoke -- --ignored --nocapture
//! ```
//!
//! `CITRATE_SMOKE_HOLD_SECS` overrides the hold duration (default 600 —
//! the A2 acceptance criterion is 4/4 bootnodes held ≥10 min).
//!
//! The handshake uses the same code path as `EmbeddedNodeBackend::start_node`
//! (Noise_XX + HandshakeParams over the canonical genesis), so an
//! eof-after-handshake regression on any bootnode fails this test the same
//! way it broke the app in the 2026-07-04 survey.

use std::sync::Arc;
use std::time::Duration;

use citrate_network::{
    transport::HandshakeParams, NetworkTransport, NoiseKeypair, PeerManager, PeerManagerConfig,
};

/// The four testnet-beta bootnodes baked into `node_service.rs`.
const BOOTNODES: [&str; 4] = [
    "noise_f356d3ebb07371eaad371b3960272f9d58fc457cde409c34549ef03776b78141@boot1.citrate.ai:30303",
    "noise_4ed281386422f6a65b92d8760d24baa82bb1b476e9dd3e21d5a070d026802c07@boot2.citrate.ai:30303",
    "noise_2b4924671e0babc9f52eb1695c72141a9c639e17ad95a2a2d2a715eae34a420e@boot3.citrate.ai:30303",
    "noise_6ee549718d522c9ccc122585dfedef72aea8df24f4a2bb0264e7658a91118b4a@rpc.citrate.ai:30303",
];

/// Live 40204 genesis, cross-checked 2026-07-09 against
/// `eth_getBlockByNumber(0x0)` on rpc.citrate.ai (matches the A6 finding).
const LIVE_GENESIS_HASH: &str = "6b6d8b895169052cad2d48bb39f4912566bd088157143087a360d00593f63e2f";

/// Compute the canonical genesis exactly as `start_node` does (steps 1–4 of
/// its genesis block), so the handshake presents the same identity the app
/// would.
fn canonical_genesis_hash() -> citrate_consensus::types::Hash {
    let mut genesis = citrate_economics::genesis::create_canonical_genesis_block(
        citrate_economics::genesis::CANONICAL_GENESIS_TIMESTAMP,
    );
    let state_db = Arc::new(citrate_execution::StateDB::new());
    let executor = Arc::new(citrate_execution::executor::Executor::new(state_db));
    let state_root_bytes = citrate_economics::genesis::initialize_shared_genesis_state(
        &executor,
        &citrate_economics::genesis::GenesisConfig::testnet_beta(),
    );
    genesis.state_root = citrate_consensus::types::Hash::new(state_root_bytes);
    genesis.header.block_hash =
        citrate_economics::genesis::calculate_canonical_block_hash(&genesis);
    genesis.header.block_hash
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live network: run with --ignored (WP-A2 verification / C3 smoke lane)"]
async fn all_four_bootnodes_connect_and_hold() {
    let hold_secs: u64 = std::env::var("CITRATE_SMOKE_HOLD_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);

    let genesis_hash = canonical_genesis_hash();
    assert_eq!(
        hex::encode(genesis_hash.as_bytes()),
        LIVE_GENESIS_HASH,
        "locally computed canonical genesis drifted from live 40204 — \
         re-check the citrate-chain pin (this was the A6 failure mode)"
    );

    let peer_manager = Arc::new(PeerManager::new(PeerManagerConfig {
        max_peers: 25,
        max_inbound: 12,
        max_outbound: 13,
        peer_timeout: Duration::from_secs(30),
        ban_duration: Duration::from_secs(3600),
        score_threshold: -100,
    }));
    let noise_keypair = NoiseKeypair::generate();
    let local_peer_id = noise_keypair.derive_peer_id();
    let transport = NetworkTransport::new(
        peer_manager.clone(),
        local_peer_id,
        HandshakeParams {
            network_id: 40204,
            genesis_hash,
            // `head` is now the live, shared (height, hash) the node refreshes;
            // a handshake-only probe advertises genesis (height 0), as before.
            head: Arc::new(tokio::sync::RwLock::new((
                0,
                citrate_consensus::types::Hash::default(),
            ))),
        },
    )
    .with_noise(noise_keypair);

    transport
        .start_listener("0.0.0.0:0".parse().unwrap())
        .await
        .expect("listener");

    let mut failures = Vec::new();
    for s in BOOTNODES {
        let (_identity, addr) = citrate_network::resolve_bootnode(s)
            .await
            .unwrap_or_else(|| panic!("cannot resolve bootnode {s}"));
        match transport.connect_to(addr).await {
            Ok(()) => println!("CONNECTED {s} ({addr})"),
            Err(e) => failures.push(format!("{s} ({addr}): {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "bootnode handshake failures:\n{}",
        failures.join("\n")
    );

    // Hold phase: the A2 bug was an eof AFTER a successful handshake, so a
    // 4/4 connect alone proves nothing — the connections must survive.
    let start = std::time::Instant::now();
    let mut min_outbound = usize::MAX;
    while start.elapsed() < Duration::from_secs(hold_secs) {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let (total, _inbound, outbound) = peer_manager.get_peer_counts().await;
        min_outbound = min_outbound.min(outbound);
        println!(
            "t+{:>4}s peers total={total} outbound={outbound}",
            start.elapsed().as_secs()
        );
        assert!(
            outbound >= BOOTNODES.len(),
            "peer count dropped to {outbound}/{} after {}s — \
             eof-after-handshake regression (WP-A2)",
            BOOTNODES.len(),
            start.elapsed().as_secs()
        );
    }
    println!(
        "HELD {hold_secs}s: min outbound {min_outbound}/{} — WP-A2 negative confirmed",
        BOOTNODES.len()
    );
}
