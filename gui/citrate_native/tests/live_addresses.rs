//! SELL-S0 live address verification (D1 close-out).
//!
//! Verifies that EVERY entry in the vendored canonical address table
//! (`src/generated/addresses.json` — the exact bytes the binary embeds via
//! `include_str!` in `marketplace_client.rs`) has deployed code on the
//! connected chain, so the GUI can never transact against a stale or
//! mis-mapped address again (defect D1, superseded by the 2026-06-07
//! CREATE2 re-roll).
//!
//! Gated behind `CITRATE_RPC_URL` and skips cleanly when unset, mirroring
//! `citrate-node-agent`'s `chainio/tests/live_rpc.rs` posture:
//!
//! ```sh
//! CITRATE_RPC_URL=https://rpc.citrate.ai cargo test -p citrate-native --test live_addresses
//! ```

use std::collections::HashMap;

/// The same vendored table `marketplace_client.rs` embeds. Loading it from
/// the manifest path keeps this test pinned to the bytes that ship.
const ADDRESS_TABLE_JSON: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/addresses.json"));

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalTable {
    chain_id: u64,
    contracts: HashMap<String, String>,
    aa_stack: HashMap<String, String>,
}

fn rpc_url() -> Option<String> {
    std::env::var("CITRATE_RPC_URL").ok()
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    });
    let resp: serde_json::Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("RPC request should reach the endpoint")
        .json()
        .await
        .expect("RPC response should be JSON");
    resp.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{method} returned no result: {resp}"))
}

#[tokio::test]
async fn every_vendored_address_has_deployed_code() {
    let Some(url) = rpc_url() else {
        eprintln!("skipping: CITRATE_RPC_URL not set");
        return;
    };
    let table: CanonicalTable = serde_json::from_str(ADDRESS_TABLE_JSON)
        .expect("vendored src/generated/addresses.json parses");
    assert_eq!(table.chain_id, 40204, "vendored table is not for chain 40204");

    let client = reqwest::Client::new();

    let chain_id_hex = rpc_call(&client, &url, "eth_chainId", serde_json::json!([])).await;
    let chain_id = u64::from_str_radix(
        chain_id_hex
            .as_str()
            .expect("eth_chainId is a hex string")
            .trim_start_matches("0x"),
        16,
    )
    .expect("eth_chainId parses");
    assert_eq!(
        chain_id, table.chain_id,
        "connected chain {chain_id} is not the vendored table's chain {}",
        table.chain_id
    );

    let mut empty = Vec::new();
    let mut checked = 0usize;
    for (name, addr) in table.contracts.iter().chain(table.aa_stack.iter()) {
        let code = rpc_call(
            &client,
            &url,
            "eth_getCode",
            serde_json::json!([addr, "latest"]),
        )
        .await;
        let code = code.as_str().expect("eth_getCode is a hex string");
        if code == "0x" || code.is_empty() {
            empty.push(format!("{name} {addr}"));
        }
        checked += 1;
    }
    eprintln!("verified deployed code at {checked} vendored addresses");
    assert!(
        empty.is_empty(),
        "vendored addresses with NO deployed code on the connected chain \
         (stale table? run `bash scripts/sync-addresses.sh`): {empty:?}"
    );
}
