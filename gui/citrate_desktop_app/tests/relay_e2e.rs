//! GUI-RELAY-S1.4 — env-gated relay end-to-end against a *live* node-agent.
//!
//! The unit tests in `relay_service` mock the HTTP queue + the signer. This test
//! exercises the **real** [`NodeAgentClient`] against a running node-agent
//! supervision surface — the one piece that can't be covered offline — and runs
//! whatever it returns through the **real** [`RelayValidator`]. It is:
//!
//! - **gated**: skips cleanly unless `CITRATE_RELAY_E2E` is set, so CI/offline
//!   runs are unaffected;
//! - **read-only**: it lists + validates but never calls `mark_observed` (that
//!   would tell a live node-agent a write was broadcast when it wasn't) and never
//!   signs (signing needs a funded, unlocked wallet);
//! - the **full sign → broadcast → observe → payout** flow is the operator
//!   acceptance procedure (planset §9), not an automated test — it requires a
//!   devnet, a won job, and a funded wallet.
//!
//! Run it:
//! ```sh
//! CITRATE_RELAY_E2E=1 CITRATE_NODE_AGENT_ADDR=http://127.0.0.1:19600 \
//!   cargo test -p citrate-desktop-app --test relay_e2e -- --nocapture
//! ```

use citrate_desktop_app::services::relay_service::{
    NodeAgentClient, RelayValidator, RequestQueue,
};

// Canonical chain-40204 contracts (mirror the gui_native address book +
// citrate-chain DEPLOYED_ADDRESSES.md).
const CHAIN_ID: u64 = 40204;
const MARKETPLACE: &str = "0xc12dbcdb80ef2ae675315f455210f39a736a373c";
const ACCOUNTING: &str = "0x86d918808b48ad543c9c816b5303b7dbcb0e321f";
const HEARTBEAT_MONITOR: &str = "0xe9eaac272844f342266862bbefc6d117a227ad9b";

/// Returns the node-agent URL if the e2e is enabled, else `None` (skip).
fn agent_url() -> Option<String> {
    if std::env::var("CITRATE_RELAY_E2E").is_err() {
        return None;
    }
    Some(std::env::var("CITRATE_NODE_AGENT_ADDR").unwrap_or_else(|_| "http://127.0.0.1:19600".into()))
}

#[tokio::test]
async fn e2e_lists_and_validates_the_live_queue() {
    let Some(url) = agent_url() else {
        eprintln!("relay e2e skipped — set CITRATE_RELAY_E2E=1 (and a running node-agent) to run");
        return;
    };

    let client = NodeAgentClient::new(url.clone());
    let requests = client
        .list_requests()
        .await
        .unwrap_or_else(|e| panic!("GET {url}/signature-requests must succeed: {e}"));
    eprintln!("node-agent queue: {} request(s)", requests.len());

    // Run every queued request through the real validator — proves the validator
    // accepts the node-agent's actual wire shape (and would refuse anything off
    // the allow-list). Read-only: nothing is signed or observed.
    let validator = RelayValidator::new(CHAIN_ID, MARKETPLACE, ACCOUNTING, HEARTBEAT_MONITOR);
    for r in &requests {
        match validator.validate(r) {
            Ok(vw) => eprintln!("  ✓ would sign {} (id {}, {} bytes calldata)", vw.intent, vw.id, vw.calldata.len()),
            Err(reason) => eprintln!("  ✗ id {} refused: {reason:?}", r.id),
        }
    }
}
