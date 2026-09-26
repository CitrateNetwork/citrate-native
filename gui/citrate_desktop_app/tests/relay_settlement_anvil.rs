//! Live compatibility check of the relay's settlement reads against the
//! current `ComputeMarketplace` / `ComputeVerifier` contracts.
//!
//! Gated: runs only when `CITRATE_R2_ANVIL_ADDRS` (JSON with
//! `ComputeMarketplace` and `ComputeVerifier`) is set. `CITRATE_R2_ANVIL_RPC`
//! defaults to `http://127.0.0.1:8599`. The node must be an anvil
//! (`anvil_impersonateAccount`, `anvil_setBalance`, `anvil_mine`); the test
//! funds and impersonates its own provider and requester accounts.
//! Read-only for the relay: every write here is a test fixture sent from an
//! unlocked account, never through the wallet signer.

use citrate_desktop_app::services::relay_service::{
    RpcSettlementReader, SettlementReader, DISPUTE_WINDOW_BLOCKS,
};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

/// Test-only accounts, impersonated on anvil (no keys exist for them).
const PROVIDER: &str = "0x000000000000000000000000000000000000a11c";
const REQUESTER: &str = "0x000000000000000000000000000000000000b0b0";
/// Governance of the local deploy (anvil default account #0, unlocked).
const GOVERNANCE: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Keccak256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn sel(sig: &str) -> Vec<u8> {
    keccak(&[sig.as_bytes()])[..4].to_vec()
}

fn word(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

async fn rpc(url: &str, method: &str, params: Value) -> Result<Value, String> {
    let resp: Value = reqwest::Client::new()
        .post(url)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    match resp.get("error") {
        Some(e) => Err(e.to_string()),
        None => Ok(resp["result"].clone()),
    }
}

/// Simulate, send from an unlocked account, and require receipt status 1.
async fn send(url: &str, from: &str, to: &str, data: &[u8], value: u128) -> Result<(), String> {
    let tx = json!({"from": from, "to": to, "data": format!("0x{}", hex::encode(data)), "value": format!("0x{value:x}")});
    rpc(url, "eth_call", json!([tx.clone(), "pending"])).await?;
    let hash = rpc(url, "eth_sendTransaction", json!([tx])).await?;
    for _ in 0..100 {
        let r = rpc(url, "eth_getTransactionReceipt", json!([hash])).await?;
        if let Some(s) = r.get("status").and_then(Value::as_str) {
            return if s == "0x1" {
                Ok(())
            } else {
                Err(format!("status {s}"))
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Err("no receipt".into())
}

async fn call_u128(url: &str, to: &str, data: Vec<u8>) -> u128 {
    let out = rpc(
        url,
        "eth_call",
        json!([{"to": to, "data": format!("0x{}", hex::encode(data))}, "latest"]),
    )
    .await
    .expect("eth_call");
    u128::from_str_radix(&out.as_str().expect("hex")[2..66][32..], 16).expect("u128")
}

fn post_job(model: [u8; 32], input: &[u8], price: u128, tier: u8) -> Vec<u8> {
    let mut d = sel("postJob(bytes32,bytes,uint256,uint8,uint256,uint256)");
    d.extend_from_slice(&model);
    d.extend_from_slice(&word(6 * 32));
    d.extend_from_slice(&word(price));
    d.extend_from_slice(&word(tier as u128));
    d.extend_from_slice(&word(5));
    d.extend_from_slice(&word(200));
    d.extend_from_slice(&word(input.len() as u128));
    let mut padded = input.to_vec();
    padded.resize(input.len().div_ceil(32) * 32, 0);
    d.extend_from_slice(&padded);
    d
}

fn job_call(sig: &str, job: u128) -> Vec<u8> {
    let mut d = sel(sig);
    d.extend_from_slice(&word(job));
    d
}

/// Post a 1-SALT Commitment job, have the provider win, execute, commit and
/// (one block later) reveal. Returns the job id. With `check`, asserts the
/// reader's commitment-block / head / verified-at reads along the way.
async fn run_to_valid(
    url: &str,
    market: &str,
    reader: &RpcSettlementReader,
    model: [u8; 32],
    one: u128,
    check: bool,
) -> u128 {
    let url = url.to_string();
    let market = market.to_string();
    let job = call_u128(&url, &market, sel("nextJobId()")).await;
    send(
        &url,
        REQUESTER,
        &market,
        &post_job(model, &keccak(&[b"in"]), one, 0),
        one,
    )
    .await
    .expect("postJob");
    assert_eq!(reader.effective_tier(job).await.expect("tier"), 0);
    let mut bid = sel("bidOnJob(uint256,uint256,uint256)");
    bid.extend_from_slice(&word(job));
    bid.extend_from_slice(&word(one / 2));
    bid.extend_from_slice(&word(1_000));
    send(&url, PROVIDER, &market, &bid, 0).await.expect("bid");
    send(
        &url,
        REQUESTER,
        &market,
        &job_call("assignBestBid(uint256)", job),
        0,
    )
    .await
    .expect("assign");
    send(
        &url,
        PROVIDER,
        &market,
        &job_call("startExecution(uint256)", job),
        0,
    )
    .await
    .expect("start");

    let output = b"out";
    let nonce = keccak(&[b"n", &job.to_be_bytes()]);
    let commitment = keccak(&[output, &nonce]);
    let mut commit = job_call("submitCommitment(uint256,bytes32)", job);
    commit.extend_from_slice(&commitment);
    send(&url, PROVIDER, &market, &commit, 0)
        .await
        .expect("commit");

    let head = reader.head().await.expect("head");
    assert_eq!(
        reader.commitment_block(job).await.expect("commitmentBlock"),
        head
    );
    assert_eq!(reader.result_verified_at(job).await.expect("verifiedAt"), 0);

    rpc(&url, "anvil_mine", json!(["0x1"])).await.expect("mine");
    if check {
        assert_eq!(reader.head().await.expect("head"), head + 1);
    }
    let mut proof = commitment.to_vec();
    proof.extend_from_slice(&nonce);
    proof.extend_from_slice(output);
    let mut reveal = job_call("submitResult(uint256,bytes,bytes)", job);
    reveal.extend_from_slice(&word(0x60));
    reveal.extend_from_slice(&word(0x60 + 32 + 32));
    reveal.extend_from_slice(&word(32));
    reveal.extend_from_slice(&keccak(&[output]));
    reveal.extend_from_slice(&word(proof.len() as u128));
    let mut padded = proof.clone();
    padded.resize(proof.len().div_ceil(32) * 32, 0);
    reveal.extend_from_slice(&padded);
    send(&url, PROVIDER, &market, &reveal, 0)
        .await
        .expect("submitResult");

    job
}

#[tokio::test]
async fn settlement_reads_match_the_deployed_contracts() {
    let Ok(path) = std::env::var("CITRATE_R2_ANVIL_ADDRS") else {
        eprintln!("skipping: CITRATE_R2_ANVIL_ADDRS not set");
        return;
    };
    let url =
        std::env::var("CITRATE_R2_ANVIL_RPC").unwrap_or_else(|_| "http://127.0.0.1:8599".into());
    let book: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("addrs")).expect("json");
    let market = book["ComputeMarketplace"]
        .as_str()
        .expect("market")
        .to_string();
    let verifier = book["ComputeVerifier"]
        .as_str()
        .expect("verifier")
        .to_string();
    let reader = RpcSettlementReader::new(url.clone(), market.clone(), verifier.clone());
    let one: u128 = 1_000_000_000_000_000_000;
    for who in [PROVIDER, REQUESTER] {
        rpc(&url, "anvil_impersonateAccount", json!([who]))
            .await
            .expect("impersonate");
        rpc(
            &url,
            "anvil_setBalance",
            json!([who, format!("0x{:x}", 100_000 * one)]),
        )
        .await
        .expect("fund");
    }

    // Provider registration (idempotent across reruns).
    let model = keccak(&[b"r2-native-compat-model"]);
    let stake = call_u128(&url, &market, sel("MIN_PROVIDER_STAKE()")).await;
    let mut reg = sel("registerProvider(bytes32[])");
    reg.extend_from_slice(&word(32));
    reg.extend_from_slice(&word(1));
    reg.extend_from_slice(&model);
    let _ = send(&url, PROVIDER, &market, &reg, stake).await;

    // Commitment job, 1 SALT, driven to a Valid result.
    let job = run_to_valid(&url, &market, &reader, model, one, true).await;
    let verified_at = reader.result_verified_at(job).await.expect("verifiedAt");
    assert_eq!(verified_at, reader.head().await.expect("head"));
    assert!(!reader
        .dispute_resolved_for_provider(job)
        .await
        .expect("resolved"));
    // The contract agrees with the relay's window: completeJob reverts
    // before verifiedAt + DISPUTE_WINDOW and succeeds at it.
    assert!(send(
        &url,
        PROVIDER,
        &market,
        &job_call("completeJob(uint256)", job),
        0
    )
    .await
    .is_err());
    let now = reader.head().await.expect("head");
    let to_mine = verified_at + DISPUTE_WINDOW_BLOCKS - now - 1;
    rpc(&url, "anvil_mine", json!([format!("0x{to_mine:x}")]))
        .await
        .expect("mine");
    send(
        &url,
        PROVIDER,
        &market,
        &job_call("completeJob(uint256)", job),
        0,
    )
    .await
    .expect("completeJob once the window has elapsed");

    // A dispute resolved for the provider lifts the window: the relay's
    // flag reads true and completeJob lands at once.
    let disputed = run_to_valid(&url, &market, &reader, model, one, false).await;
    assert!(!reader
        .dispute_resolved_for_provider(disputed)
        .await
        .expect("resolved"));
    send(
        &url,
        REQUESTER,
        &market,
        &job_call("disputeResult(uint256)", disputed),
        10 * one,
    )
    .await
    .expect("dispute");
    let mut resolve = job_call("resolveDispute(uint256,bool)", disputed);
    resolve.extend_from_slice(&word(0)); // requesterWins = false
    send(&url, GOVERNANCE, &market, &resolve, 0)
        .await
        .expect("governance resolves for provider");
    assert!(reader
        .dispute_resolved_for_provider(disputed)
        .await
        .expect("resolved"));
    send(
        &url,
        PROVIDER,
        &market,
        &job_call("completeJob(uint256)", disputed),
        0,
    )
    .await
    .expect("completeJob after a provider-won dispute, inside the window");

    // Commitment request above 10 SALT reads as ZKProof (tier 1).
    let zk = call_u128(&url, &market, sel("nextJobId()")).await;
    let mut zk_model = [0u8; 32];
    zk_model[31] = 2;
    let mut zk_input = [0u8; 32];
    zk_input[31] = 1;
    send(
        &url,
        REQUESTER,
        &market,
        &post_job(zk_model, &zk_input, 11 * one, 0),
        11 * one,
    )
    .await
    .expect("post >10 SALT job");
    assert_eq!(reader.effective_tier(zk).await.expect("tier"), 1);

    // A read against a non-contract fails rather than returning a default.
    let bogus = RpcSettlementReader::new(
        url.clone(),
        "0x0000000000000000000000000000000000000001",
        verifier,
    );
    assert!(bogus.result_verified_at(job).await.is_err());
}
