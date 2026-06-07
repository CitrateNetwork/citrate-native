//! `relay_service` — the SELL-S2 signing relay (GUI-RELAY-S1).
//!
//! The node-agent runs a provider's won jobs and emits the next **unsigned**
//! chain write into its supervision queue (`GET /signature-requests`). It holds
//! no keys. This service is the keystore half: poll that queue, **validate** each
//! write against a strict allow-list, **sign + broadcast** it via the wallet's
//! existing signer, and **report it observed** (`POST /signature-requests/{id}/observed`).
//!
//! This module is headless + fully unit-tested:
//! - `run_once` depends on two narrow traits ([`TxSigner`], [`RequestQueue`]) so
//!   the whole poll→validate→sign→observe flow is exercised with mocks.
//! - [`NodeAgentClient`] is the real `reqwest` queue client; [`WalletTxSigner`]
//!   adapts the production [`WalletBackend`] (the UI loop wires those in S1.3).
//!
//! Safety (ADR-agent-signing): the relay signs **only** the five known SELL-S2
//! writes, to the two known contracts, with value 0. Anything else is refused +
//! recorded, never signed (the trust boundary against a compromised node-agent).

use std::sync::Arc;

use serde::Deserialize;

use crate::services::wallet_service::WalletService;

// ── the node-agent contract ────────────────────────────────────────────────
// Mirrors `citrate-node-agent` `supervision::PendingSignatureRequest`.

/// One queued unsigned chain write from the node-agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PendingRequest {
    pub id: u64,
    pub intent: String,
    pub to: String,
    pub calldata: String,
    /// Decimal string (a `u128`; JSON-number would lose precision).
    pub value_wei: String,
    pub chain_id: u64,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub expires_block: String,
    /// `"pending"` (awaiting signing) or `"submitted"` (already broadcast).
    pub status: String,
    #[serde(default)]
    pub tx_hash: Option<String>,
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Failure talking to the node-agent queue.
#[derive(Debug)]
pub enum RelayError {
    /// Transport / non-2xx HTTP.
    Http(String),
    /// Response body couldn't be decoded.
    Decode(String),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::Http(m) => write!(f, "node-agent http error: {m}"),
            RelayError::Decode(m) => write!(f, "node-agent decode error: {m}"),
        }
    }
}

impl std::error::Error for RelayError {}

// ── seams (mockable) ──────────────────────────────────────────────────────────

/// The node-agent signature-request queue.
#[async_trait::async_trait]
pub trait RequestQueue: Send + Sync {
    /// All queued requests (pending + submitted).
    async fn list_requests(&self) -> Result<Vec<PendingRequest>, RelayError>;
    /// Report a request signed + broadcast with its `tx_hash`.
    async fn mark_observed(&self, id: u64, tx_hash: &str) -> Result<(), RelayError>;
}

/// Signs + broadcasts a contract call, returning the tx hash. Narrow seam over
/// the wallet so `run_once` is testable without a keystore.
#[async_trait::async_trait]
pub trait TxSigner: Send + Sync {
    async fn sign_and_send(
        &self,
        from: &str,
        to: &str,
        value_wei: &str,
        data: Vec<u8>,
    ) -> Result<String, String>;
}

// ── real implementations (wired by the UI loop in S1.3) ───────────────────────

/// `reqwest` client for the node-agent supervision surface (loopback).
pub struct NodeAgentClient {
    base_url: String,
    http: reqwest::Client,
}

impl NodeAgentClient {
    /// `base_url` e.g. `http://127.0.0.1:19600` (`CITRATE_NODE_AGENT_ADDR`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl RequestQueue for NodeAgentClient {
    async fn list_requests(&self) -> Result<Vec<PendingRequest>, RelayError> {
        let url = format!("{}/signature-requests", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| RelayError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(RelayError::Http(format!("status {}", resp.status())));
        }
        resp.json::<Vec<PendingRequest>>()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))
    }

    async fn mark_observed(&self, id: u64, tx_hash: &str) -> Result<(), RelayError> {
        let url = format!(
            "{}/signature-requests/{}/observed",
            self.base_url.trim_end_matches('/'),
            id
        );
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "tx_hash": tx_hash }))
            .send()
            .await
            .map_err(|e| RelayError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(RelayError::Http(format!("status {}", resp.status())));
        }
        Ok(())
    }
}

/// Adapts the production [`WalletService`] to [`TxSigner`] (reuses its existing
/// `send_transaction_with_data` — session check + value-reauth + nonce-fetch →
/// `TransactionBuilder` sign → `eth_sendRawTransaction`; **no new keystore code**).
/// The empty password relies on an already-unlocked session; the service refuses
/// (`SessionExpired`) if it isn't — a hard backstop under the relay's unlock gate.
pub struct WalletTxSigner {
    wallet: Arc<WalletService>,
}

impl WalletTxSigner {
    pub fn new(wallet: Arc<WalletService>) -> Self {
        Self { wallet }
    }
}

#[async_trait::async_trait]
impl TxSigner for WalletTxSigner {
    async fn sign_and_send(
        &self,
        from: &str,
        to: &str,
        value_wei: &str,
        data: Vec<u8>,
    ) -> Result<String, String> {
        self.wallet
            .send_transaction_with_data(from, to, value_wei, data, "")
            .await
            .map_err(|e| e.to_string())
    }
}

// ── validation (the trust boundary) ───────────────────────────────────────────

/// 4-byte selectors of the five writes the relay will sign (mirror
/// `citrate-node-agent` `chainio::selectors`, pinned).
const SEL_START_EXECUTION: [u8; 4] = [0xc7, 0x8e, 0xc1, 0x8e];
const SEL_SUBMIT_COMMITMENT: [u8; 4] = [0xe6, 0xa3, 0xd9, 0xdc];
const SEL_SUBMIT_RESULT: [u8; 4] = [0xba, 0xa2, 0xc0, 0x78];
const SEL_COMPLETE_JOB: [u8; 4] = [0xa1, 0xc0, 0xd3, 0x2f];
const SEL_CLAIM_REWARDS: [u8; 4] = [0x37, 0x25, 0x00, 0xab];

/// Why the relay refused to sign a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Not in `pending` status (already submitted / unknown state).
    NotPending,
    /// Wrong chain id.
    WrongChain { expected: u64, got: u64 },
    /// Non-zero value — these writes are all non-payable.
    NonZeroValue(String),
    /// Intent isn't one of the five SELL-S2 writes.
    UnknownIntent(String),
    /// `to` isn't the contract this intent must target.
    WrongContract { intent: String, to: String },
    /// Calldata wasn't valid hex.
    BadCalldata(String),
    /// Calldata's 4-byte selector doesn't match the intent.
    SelectorMismatch { intent: String },
}

/// A request that passed every check — safe to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWrite {
    pub id: u64,
    pub intent: String,
    pub to: String,
    pub value_wei: String,
    pub calldata: Vec<u8>,
}

/// Validates queued requests against the SELL-S2 allow-list before signing.
pub struct RelayValidator {
    /// The chain the wallet is signing for (40204).
    pub chain_id: u64,
    /// `ComputeMarketplace` address (job-lifecycle writes target this).
    pub marketplace: String,
    /// `ContributionAccounting` address (`claimRewards` targets this).
    pub accounting: String,
}

impl RelayValidator {
    pub fn new(chain_id: u64, marketplace: impl Into<String>, accounting: impl Into<String>) -> Self {
        Self {
            chain_id,
            marketplace: marketplace.into(),
            accounting: accounting.into(),
        }
    }

    /// Accept a request only if every guard holds; otherwise the reason it was refused.
    pub fn validate(&self, req: &PendingRequest) -> Result<ValidatedWrite, RejectReason> {
        if req.status != "pending" {
            return Err(RejectReason::NotPending);
        }
        if req.chain_id != self.chain_id {
            return Err(RejectReason::WrongChain {
                expected: self.chain_id,
                got: req.chain_id,
            });
        }
        if req.value_wei != "0" {
            return Err(RejectReason::NonZeroValue(req.value_wei.clone()));
        }
        let (expected_contract, expected_selector) = match req.intent.as_str() {
            "startExecution" => (&self.marketplace, SEL_START_EXECUTION),
            "submitCommitment" => (&self.marketplace, SEL_SUBMIT_COMMITMENT),
            "submitResult" => (&self.marketplace, SEL_SUBMIT_RESULT),
            "completeJob" => (&self.marketplace, SEL_COMPLETE_JOB),
            "claimRewards" => (&self.accounting, SEL_CLAIM_REWARDS),
            other => return Err(RejectReason::UnknownIntent(other.to_string())),
        };
        if !addr_eq(&req.to, expected_contract) {
            return Err(RejectReason::WrongContract {
                intent: req.intent.clone(),
                to: req.to.clone(),
            });
        }
        let calldata =
            decode_hex(&req.calldata).map_err(RejectReason::BadCalldata)?;
        if calldata.len() < 4 || calldata[0..4] != expected_selector {
            return Err(RejectReason::SelectorMismatch {
                intent: req.intent.clone(),
            });
        }
        Ok(ValidatedWrite {
            id: req.id,
            intent: req.intent.clone(),
            to: req.to.clone(),
            value_wei: req.value_wei.clone(),
            calldata,
        })
    }
}

// ── the tick ──────────────────────────────────────────────────────────────────

/// A request signed + broadcast this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedWrite {
    pub id: u64,
    pub intent: String,
    pub tx_hash: String,
}

/// What one [`run_once`] did (for logging + the UI toast).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelayTickReport {
    /// Writes signed + broadcast + reported observed.
    pub signed: Vec<SignedWrite>,
    /// Requests refused by the validator (id + reason).
    pub rejected: Vec<(u64, RejectReason)>,
    /// Transient errors (queue unreachable, broadcast failed, observe failed).
    pub errors: Vec<String>,
    /// Already-submitted entries skipped.
    pub skipped_non_pending: usize,
}

/// Run one relay cycle: poll the queue, and sign **at most one** valid pending
/// write (one per tick keeps account-nonce handling simple; the node-agent dedups
/// re-emits so it still converges). Rejected/already-submitted requests are
/// recorded, never signed. `observed` is POSTed only after a real broadcast hash.
pub async fn run_once(
    signer: &dyn TxSigner,
    from: &str,
    queue: &dyn RequestQueue,
    validator: &RelayValidator,
) -> RelayTickReport {
    let mut report = RelayTickReport::default();

    let requests = match queue.list_requests().await {
        Ok(r) => r,
        Err(e) => {
            report.errors.push(format!("list requests: {e}"));
            return report;
        }
    };

    for req in &requests {
        match validator.validate(req) {
            Err(RejectReason::NotPending) => report.skipped_non_pending += 1,
            Err(reason) => report.rejected.push((req.id, reason)),
            Ok(vw) => {
                match signer
                    .sign_and_send(from, &vw.to, &vw.value_wei, vw.calldata.clone())
                    .await
                {
                    Ok(tx_hash) => {
                        // Report observed only after a real broadcast hash.
                        if let Err(e) = queue.mark_observed(vw.id, &tx_hash).await {
                            report.errors.push(format!("observe {}: {e}", vw.id));
                        }
                        report.signed.push(SignedWrite {
                            id: vw.id,
                            intent: vw.intent,
                            tx_hash,
                        });
                        // One signed write per tick — stop here.
                        break;
                    }
                    Err(e) => {
                        // Leave it pending for retry; don't sign more this tick.
                        report.errors.push(format!("sign {} ({}): {e}", vw.id, vw.intent));
                        break;
                    }
                }
            }
        }
    }

    report
}

// ── service (owns the opt-in toggle + config; the UI loop drives ticks) ───────

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Static config for a relay deployment.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// node-agent supervision base URL (loopback), e.g. `http://127.0.0.1:19600`.
    pub agent_url: String,
    /// Chain the wallet signs for (40204).
    pub chain_id: u64,
    /// `ComputeMarketplace` address.
    pub marketplace: String,
    /// `ContributionAccounting` address.
    pub accounting: String,
    /// How often the UI loop should call [`RelayService::tick`].
    pub poll_interval: Duration,
}

/// Owns the relay's opt-in toggle + config. The UI spawns a loop that calls
/// [`RelayService::tick`] every `poll_interval`; `tick` is a **no-op unless the
/// relay is enabled AND the wallet is unlocked** (the custody gate — the relay
/// never auto-unlocks). All the real work is the unit-tested [`run_once`].
pub struct RelayService {
    config: RelayConfig,
    enabled: AtomicBool,
}

impl RelayService {
    /// Create disabled by default (opt-in — the user flips the toggle).
    pub fn new(config: RelayConfig) -> Self {
        Self {
            config,
            enabled: AtomicBool::new(false),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    fn validator(&self) -> RelayValidator {
        RelayValidator::new(self.config.chain_id, &self.config.marketplace, &self.config.accounting)
    }

    /// Run one gated cycle. Returns `None` when the relay is disabled or the
    /// wallet is locked (nothing signed); otherwise the [`RelayTickReport`].
    pub async fn tick(
        &self,
        signer: &dyn TxSigner,
        from: &str,
        queue: &dyn RequestQueue,
        unlocked: bool,
    ) -> Option<RelayTickReport> {
        if !self.is_enabled() || !unlocked {
            return None;
        }
        Some(run_once(signer, from, queue, &self.validator()).await)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Case-insensitive `0x`-hex address equality.
fn addr_eq(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_start_matches("0x").trim_start_matches("0X").to_lowercase();
    n(a) == n(b)
}

/// Decode a `0x`-prefixed (or bare) hex string into bytes.
fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let h = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if !h.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| "non-hex digit".to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const MARKETPLACE: &str = "0xf3f9f72ea2bb3f763b07390b7257da643b8ee9b6";
    const ACCOUNTING: &str = "0x1afe987622ab5add275d2fd21248f77f5e00667f";
    const FROM: &str = "0xabababababababababababababababababababab";

    fn validator() -> RelayValidator {
        RelayValidator::new(40204, MARKETPLACE, ACCOUNTING)
    }

    /// A pending request with a given intent, contract, value, and selector.
    fn req(id: u64, intent: &str, to: &str, value_wei: &str, selector: [u8; 4]) -> PendingRequest {
        let mut calldata = format!("0x{:02x}{:02x}{:02x}{:02x}", selector[0], selector[1], selector[2], selector[3]);
        calldata.push_str(&"00".repeat(32)); // a 32-byte arg, plausible calldata
        PendingRequest {
            id,
            intent: intent.to_string(),
            to: to.to_string(),
            calldata,
            value_wei: value_wei.to_string(),
            chain_id: 40204,
            context: format!("{intent} job {id}"),
            expires_block: "1000".to_string(),
            status: "pending".to_string(),
            tx_hash: None,
        }
    }

    // ---- S1.1: deserialization + validator ----

    #[test]
    fn deserializes_node_agent_queue_json() {
        let json = r#"[
          {"id":0,"intent":"submitResult","to":"0xF3F9F72EA2BB3F763B07390B7257DA643B8EE9B6",
           "calldata":"0xbaa2c078dead","value_wei":"0","chain_id":40204,
           "context":"submitResult job 7","expires_block":"1000","status":"pending"}
        ]"#;
        let reqs: Vec<PendingRequest> = serde_json::from_str(json).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].intent, "submitResult");
        assert_eq!(reqs[0].value_wei, "0");
        assert_eq!(reqs[0].tx_hash, None);
    }

    #[test]
    fn validator_accepts_each_known_write() {
        let v = validator();
        for (intent, to, sel) in [
            ("startExecution", MARKETPLACE, SEL_START_EXECUTION),
            ("submitCommitment", MARKETPLACE, SEL_SUBMIT_COMMITMENT),
            ("submitResult", MARKETPLACE, SEL_SUBMIT_RESULT),
            ("completeJob", MARKETPLACE, SEL_COMPLETE_JOB),
            ("claimRewards", ACCOUNTING, SEL_CLAIM_REWARDS),
        ] {
            let r = req(1, intent, to, "0", sel);
            let vw = v.validate(&r).unwrap_or_else(|e| panic!("{intent} should pass: {e:?}"));
            assert_eq!(vw.intent, intent);
            assert_eq!(&vw.calldata[0..4], &sel);
        }
    }

    #[test]
    fn validator_accepts_mixed_case_address() {
        let v = validator();
        let r = req(1, "submitResult", "0xF3F9F72EA2BB3F763B07390B7257DA643B8EE9B6", "0", SEL_SUBMIT_RESULT);
        assert!(v.validate(&r).is_ok());
    }

    #[test]
    fn validator_refuses_non_pending() {
        let v = validator();
        let mut r = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        r.status = "submitted".into();
        assert_eq!(v.validate(&r), Err(RejectReason::NotPending));
    }

    #[test]
    fn validator_refuses_wrong_chain() {
        let v = validator();
        let mut r = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        r.chain_id = 1;
        assert_eq!(v.validate(&r), Err(RejectReason::WrongChain { expected: 40204, got: 1 }));
    }

    #[test]
    fn validator_refuses_nonzero_value() {
        let v = validator();
        let r = req(1, "submitResult", MARKETPLACE, "5", SEL_SUBMIT_RESULT);
        assert_eq!(v.validate(&r), Err(RejectReason::NonZeroValue("5".into())));
    }

    #[test]
    fn validator_refuses_unknown_intent() {
        let v = validator();
        let r = req(1, "drainTreasury", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        assert_eq!(v.validate(&r), Err(RejectReason::UnknownIntent("drainTreasury".into())));
    }

    #[test]
    fn validator_refuses_wrong_contract() {
        let v = validator();
        // claimRewards must target accounting, not the marketplace.
        let r = req(1, "claimRewards", MARKETPLACE, "0", SEL_CLAIM_REWARDS);
        assert!(matches!(v.validate(&r), Err(RejectReason::WrongContract { .. })));
        // an attacker-supplied 'to' is refused.
        let r2 = req(1, "submitResult", "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "0", SEL_SUBMIT_RESULT);
        assert!(matches!(v.validate(&r2), Err(RejectReason::WrongContract { .. })));
    }

    #[test]
    fn validator_refuses_selector_mismatch() {
        let v = validator();
        // intent says submitResult but the calldata carries completeJob's selector.
        let r = req(1, "submitResult", MARKETPLACE, "0", SEL_COMPLETE_JOB);
        assert_eq!(v.validate(&r), Err(RejectReason::SelectorMismatch { intent: "submitResult".into() }));
    }

    #[test]
    fn validator_refuses_bad_calldata() {
        let v = validator();
        let mut r = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        r.calldata = "0xZZ".into();
        assert!(matches!(v.validate(&r), Err(RejectReason::BadCalldata(_))));
    }

    // ---- S1.2: run_once with mocks ----

    struct MockQueue {
        requests: Vec<PendingRequest>,
        observed: Mutex<Vec<(u64, String)>>,
        fail_observe: bool,
    }
    impl MockQueue {
        fn new(requests: Vec<PendingRequest>) -> Self {
            Self { requests, observed: Mutex::new(Vec::new()), fail_observe: false }
        }
    }
    #[async_trait::async_trait]
    impl RequestQueue for MockQueue {
        async fn list_requests(&self) -> Result<Vec<PendingRequest>, RelayError> {
            Ok(self.requests.clone())
        }
        async fn mark_observed(&self, id: u64, tx_hash: &str) -> Result<(), RelayError> {
            if self.fail_observe {
                return Err(RelayError::Http("observe boom".into()));
            }
            self.observed.lock().unwrap().push((id, tx_hash.to_string()));
            Ok(())
        }
    }

    struct MockSigner {
        ok: bool,
        calls: Mutex<Vec<(String, String, String, Vec<u8>)>>,
    }
    impl MockSigner {
        fn new(ok: bool) -> Self {
            Self { ok, calls: Mutex::new(Vec::new()) }
        }
    }
    #[async_trait::async_trait]
    impl TxSigner for MockSigner {
        async fn sign_and_send(&self, from: &str, to: &str, value_wei: &str, data: Vec<u8>) -> Result<String, String> {
            self.calls.lock().unwrap().push((from.into(), to.into(), value_wei.into(), data));
            if self.ok {
                Ok("0xdeadbeef".to_string())
            } else {
                Err("broadcast failed: node down".to_string())
            }
        }
    }

    #[tokio::test]
    async fn run_once_signs_validates_and_observes() {
        let q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator()).await;

        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 7);
        assert_eq!(report.signed[0].tx_hash, "0xdeadbeef");
        // It signed against the right account + contract + value.
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls[0].0, FROM);
        assert_eq!(calls[0].2, "0");
        // It POSTed observed with the hash.
        assert_eq!(q.observed.lock().unwrap().as_slice(), &[(7, "0xdeadbeef".to_string())]);
    }

    #[tokio::test]
    async fn run_once_broadcast_failure_leaves_it_pending() {
        let q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        let s = MockSigner::new(false); // broadcast fails
        let report = run_once(&s, FROM, &q, &validator()).await;

        assert!(report.signed.is_empty());
        assert_eq!(report.errors.len(), 1);
        // Crucially: no observed POST without a real hash.
        assert!(q.observed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_once_skips_submitted_and_refuses_bad_then_signs_valid() {
        let mut submitted = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        submitted.status = "submitted".into();
        let bad_to = req(2, "submitResult", "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "0", SEL_SUBMIT_RESULT);
        let good = req(3, "claimRewards", ACCOUNTING, "0", SEL_CLAIM_REWARDS);
        let q = MockQueue::new(vec![submitted, bad_to, good]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator()).await;

        assert_eq!(report.skipped_non_pending, 1);
        assert_eq!(report.rejected.len(), 1);
        assert!(matches!(report.rejected[0], (2, RejectReason::WrongContract { .. })));
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 3);
    }

    #[tokio::test]
    async fn run_once_signs_at_most_one_per_tick() {
        let q = MockQueue::new(vec![
            req(1, "startExecution", MARKETPLACE, "0", SEL_START_EXECUTION),
            req(2, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT),
        ]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator()).await;
        // Only the first valid write is signed (nonce safety); #2 waits for next tick.
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 1);
        assert_eq!(s.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_once_records_observe_failure_but_still_reports_signed() {
        let mut q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        q.fail_observe = true;
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator()).await;
        // The tx was broadcast (signed recorded) even though observe POST failed.
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("observe"));
    }

    fn relay_service() -> RelayService {
        RelayService::new(RelayConfig {
            agent_url: "http://127.0.0.1:19600".into(),
            chain_id: 40204,
            marketplace: MARKETPLACE.into(),
            accounting: ACCOUNTING.into(),
            poll_interval: std::time::Duration::from_secs(5),
        })
    }

    #[tokio::test]
    async fn service_tick_is_noop_when_disabled() {
        let svc = relay_service(); // disabled by default (opt-in)
        assert!(!svc.is_enabled());
        let q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        let s = MockSigner::new(true);
        // Even unlocked, a disabled relay signs nothing.
        assert!(svc.tick(&s, FROM, &q, true).await.is_none());
        assert!(s.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_tick_is_noop_when_locked() {
        let svc = relay_service();
        svc.set_enabled(true);
        let q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        let s = MockSigner::new(true);
        // Enabled but locked → nothing signed (never auto-unlocks).
        assert!(svc.tick(&s, FROM, &q, false).await.is_none());
        assert!(s.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_tick_runs_when_enabled_and_unlocked() {
        let svc = relay_service();
        svc.set_enabled(true);
        let q = MockQueue::new(vec![req(7, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT)]);
        let s = MockSigner::new(true);
        let report = svc.tick(&s, FROM, &q, true).await.expect("should run");
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 7);
    }

    #[tokio::test]
    async fn run_once_queue_unreachable_is_an_error_not_a_panic() {
        struct DeadQueue;
        #[async_trait::async_trait]
        impl RequestQueue for DeadQueue {
            async fn list_requests(&self) -> Result<Vec<PendingRequest>, RelayError> {
                Err(RelayError::Http("connection refused".into()))
            }
            async fn mark_observed(&self, _id: u64, _tx: &str) -> Result<(), RelayError> {
                Ok(())
            }
        }
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &DeadQueue, &validator()).await;
        assert!(report.signed.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("list requests"));
    }
}
