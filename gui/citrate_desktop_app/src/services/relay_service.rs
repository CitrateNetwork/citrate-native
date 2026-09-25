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
//! Safety (ADR-agent-signing): the relay signs **only** the seven known SELL
//! writes — the five SELL-S2 job-lifecycle/claim writes plus the SELL-S1
//! `bidOnJob` (Commitment-cap-bounded) and recurring `heartbeat()` liveness
//! write — to the three known contracts, with value 0. Anything else is
//! refused + recorded, never signed (the trust boundary against a compromised
//! node-agent).

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

/// Env var pointing at the node-agent supervision token file (shared contract
/// with the node-agent, FUA-NODE-AGENT-01/02).
pub const NODE_AGENT_TOKEN_PATH_ENV: &str = "CITRATE_NODE_AGENT_TOKEN_FILE";

/// Resolve the supervision token path the same way the node-agent does.
fn supervision_token_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var(NODE_AGENT_TOKEN_PATH_ENV) {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home)
        .join(".citrate")
        .join("node-agent")
        .join("supervision.token")
}

/// Read the per-instance supervision bearer token, if present.
fn load_supervision_token() -> Option<String> {
    std::fs::read_to_string(supervision_token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// FUA-GUI-02: the relay must only ever talk to a node-agent on **loopback**.
/// Reject a remote host (confused-deputy / a relay pointed at someone else's
/// node) and a non-`http` scheme (the loopback queue is plaintext-local; an
/// `https://evil.example` override must not be honored). Returns the reason on
/// rejection.
pub fn validate_node_agent_url(base_url: &str) -> Result<(), RelayError> {
    let url = base_url.trim();
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        RelayError::Http(format!("node-agent url must be http://loopback: {url:?}"))
    })?;
    // The authority is everything up to the first path/query/fragment
    // delimiter. NAT-B-008: a hand-rolled `rsplit_once(':')` treated
    // `127.0.0.1:19600@evil.example` as host `127.0.0.1` while reqwest
    // would resolve `evil.example` — the userinfo `@` is the confused
    // deputy. Split the authority on EVERY RFC-3986 delimiter and reject
    // any userinfo component outright.
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or(rest);
    if authority.contains('@') {
        return Err(RelayError::Http(format!(
            "refusing a node-agent url with userinfo (`@`): {url:?}"
        )));
    }
    if authority.is_empty() {
        return Err(RelayError::Http(format!(
            "node-agent url has no host: {url:?}"
        )));
    }
    // Strip an optional `:port`. IPv6 literals are bracketed (`[::1]:port`),
    // so only split on the LAST colon that follows a `]` or when there are
    // no brackets at all.
    let host = if let Some(rest_after_bracket) = authority.strip_prefix('[') {
        // `[::1]` or `[::1]:port`
        match rest_after_bracket.split_once(']') {
            Some((inner, _port)) => inner,
            None => {
                return Err(RelayError::Http(format!(
                    "malformed IPv6 authority: {url:?}"
                )))
            }
        }
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    let is_loopback = host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if !is_loopback {
        return Err(RelayError::Http(format!(
            "refusing to poll a non-loopback node-agent: {url:?}"
        )));
    }
    Ok(())
}

/// `reqwest` client for the node-agent supervision surface (loopback).
pub struct NodeAgentClient {
    base_url: String,
    http: reqwest::Client,
    /// Per-instance bearer token presented on every request (FUA-NODE-AGENT-01).
    token: Option<String>,
}

impl NodeAgentClient {
    /// `base_url` e.g. `http://127.0.0.1:19600` (`CITRATE_NODE_AGENT_ADDR`).
    /// Loads the shared supervision token (if present) so the gated node-agent
    /// surface accepts our requests. Does NOT enforce loopback — use
    /// [`NodeAgentClient::try_new`] on the production path for that.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
            token: load_supervision_token(),
        }
    }

    /// Production constructor: enforce loopback (FUA-GUI-02) before building.
    ///
    /// NAT-B-009: also FAIL CLOSED when no supervision token is present.
    /// Pre-fix, an absent/empty `~/.citrate/node-agent/supervision.token`
    /// left `token: None` and `authed` sent every request unauthenticated —
    /// so any unprivileged local process that bound `127.0.0.1:19600` first
    /// could serve crafted signature-requests to an enabled relay. The
    /// bearer token is the only thing binding the queue to the real
    /// node-agent, so refuse to construct the production client without it.
    pub fn try_new(base_url: impl Into<String>) -> Result<Self, RelayError> {
        let base_url = base_url.into();
        validate_node_agent_url(&base_url)?;
        let token = load_supervision_token().ok_or_else(|| {
            RelayError::Http(
                "refusing to start the relay: no node-agent supervision token \
                 (~/.citrate/node-agent/supervision.token missing or empty) — \
                 the relay must not poll an unauthenticated loopback queue"
                    .to_string(),
            )
        })?;
        Ok(Self {
            base_url,
            http: reqwest::Client::new(),
            token: Some(token),
        })
    }

    /// Attach the bearer token to a request builder when we have one.
    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }
}

#[async_trait::async_trait]
impl RequestQueue for NodeAgentClient {
    async fn list_requests(&self) -> Result<Vec<PendingRequest>, RelayError> {
        let url = format!("{}/signature-requests", self.base_url.trim_end_matches('/'));
        let resp = self
            .authed(self.http.get(&url))
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
            .authed(self.http.post(&url))
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

/// 4-byte selectors of the seven writes the relay will sign (mirror
/// `citrate-node-agent` `chainio::selectors`, pinned).
const SEL_START_EXECUTION: [u8; 4] = [0xc7, 0x8e, 0xc1, 0x8e];
const SEL_SUBMIT_COMMITMENT: [u8; 4] = [0xe6, 0xa3, 0xd9, 0xdc];
const SEL_SUBMIT_RESULT: [u8; 4] = [0xba, 0xa2, 0xc0, 0x78];
const SEL_COMPLETE_JOB: [u8; 4] = [0xa1, 0xc0, 0xd3, 0x2f];
const SEL_CLAIM_REWARDS: [u8; 4] = [0x37, 0x25, 0x00, 0xab];
/// `bidOnJob(uint256,uint256,uint256)` — the SELL-S1 bid write.
const SEL_BID_ON_JOB: [u8; 4] = [0x18, 0x36, 0x0f, 0xc2];
/// `heartbeat()` — the recurring SELL-S1 liveness write.
const SEL_HEARTBEAT: [u8; 4] = [0x3d, 0xef, 0xb9, 0x62];

/// The bidder's Commitment-tier cap: jobs priced ≥ 10 SALT auto-upgrade to the
/// ZK tier on-chain (`ComputeVerifier.VALUE_THRESHOLD`), which the agent cannot
/// serve. A bid at or above this is never legitimate — refuse to sign it
/// (defense-in-depth mirroring the node-agent's own `COMMITMENT_CAP_WEI`).
const COMMITMENT_CAP_WEI: u128 = 10_000_000_000_000_000_000;

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
    /// Calldata arguments don't match the intent's ABI shape
    /// (FUA-GUI-01 residual, WP 6.4b).
    MalformedArgs { intent: String, reason: String },
    /// A privileged/value-bearing write the user did not confirm
    /// (FUA-GUI-01 residual, WP 6.4b).
    NotConfirmed { intent: String },
}

/// Decoded, shape-validated calldata arguments for one allow-listed write
/// (FUA-GUI-01 residual). What the relay knows it is signing — and what a
/// confirmation surface displays — instead of opaque selector-prefixed bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedArgs {
    /// `startExecution(uint256)` / `completeJob(uint256)`.
    JobId { job_id: u128 },
    /// `submitCommitment(uint256,bytes32)`.
    Commitment { job_id: u128, commitment: [u8; 32] },
    /// `submitResult(uint256,bytes,bytes)` — payload sizes, not contents.
    Result {
        job_id: u128,
        result_len: usize,
        proof_len: usize,
    },
    /// `claimRewards()` / `heartbeat()`.
    NoArgs,
    /// `bidOnJob(uint256,uint256,uint256)`.
    Bid {
        job_id: u128,
        price_wei: u128,
        latency_ms: u128,
    },
}

/// Sanity bound on relay-signed calldata. The largest legitimate write is
/// `submitResult` carrying a result + proof payload; anything beyond this is
/// not a plausible SELL-S2 job write.
const MAX_CALLDATA_BYTES: usize = 128 * 1024;

/// Job counters on `ComputeMarketplace` are small sequential integers. A
/// 256-bit word with non-zero high bytes is not a plausible job id — refuse
/// it instead of signing an arbitrary attacker-chosen value.
fn decode_job_id(word: &[u8]) -> Result<u128, String> {
    if word.len() != 32 {
        return Err("job id word is not 32 bytes".to_string());
    }
    if word[..16].iter().any(|b| *b != 0) {
        return Err("job id exceeds u128 — not a plausible job counter".to_string());
    }
    Ok(u128::from_be_bytes(
        word[16..32].try_into().expect("16 bytes"),
    ))
}

/// Decode a 32-byte ABI word as a `u128`, refusing non-zero high bytes
/// (no legitimate SELL write carries a value beyond u128).
fn decode_u128_word(word: &[u8], what: &str) -> Result<u128, String> {
    if word.len() != 32 {
        return Err(format!("{what} word is not 32 bytes"));
    }
    if word[..16].iter().any(|b| *b != 0) {
        return Err(format!("{what} exceeds u128 — not a plausible value"));
    }
    Ok(u128::from_be_bytes(
        word[16..32].try_into().expect("16 bytes"),
    ))
}

/// Round up to the next 32-byte ABI word boundary.
fn pad32(n: usize) -> usize {
    n.div_ceil(32) * 32
}

/// ABI-decode + shape-validate the arguments of an allow-listed write
/// (FUA-GUI-01 residual). Strict: exact static lengths, canonical dynamic
/// offsets, no trailing bytes. Anything else errs — the relay never signs
/// calldata it cannot fully account for.
pub fn decode_args(intent: &str, calldata: &[u8]) -> Result<DecodedArgs, String> {
    let args = &calldata[4..]; // caller has already pinned the selector
    let word = |i: usize| &args[i * 32..(i + 1) * 32];
    match intent {
        "startExecution" | "completeJob" => {
            if args.len() != 32 {
                return Err(format!(
                    "{intent}(uint256) takes exactly one word, got {} bytes",
                    args.len()
                ));
            }
            Ok(DecodedArgs::JobId {
                job_id: decode_job_id(word(0))?,
            })
        }
        "submitCommitment" => {
            if args.len() != 64 {
                return Err(format!(
                    "submitCommitment(uint256,bytes32) takes exactly two words, got {} bytes",
                    args.len()
                ));
            }
            let commitment: [u8; 32] = word(1).try_into().expect("32 bytes");
            Ok(DecodedArgs::Commitment {
                job_id: decode_job_id(word(0))?,
                commitment,
            })
        }
        "submitResult" => {
            // submitResult(uint256,bytes,bytes) — canonical head + two
            // in-bounds, back-to-back dynamic tails.
            if args.len() < 96 {
                return Err("submitResult head requires three words".to_string());
            }
            let job_id = decode_job_id(word(0))?;
            let off_result =
                decode_job_id(word(1)).map_err(|_| "result offset overflows".to_string())? as usize;
            let off_proof =
                decode_job_id(word(2)).map_err(|_| "proof offset overflows".to_string())? as usize;
            if off_result != 0x60 {
                return Err(format!(
                    "non-canonical result offset {off_result:#x} (expected 0x60)"
                ));
            }
            let read_len = |off: usize| -> Result<usize, String> {
                if off + 32 > args.len() {
                    return Err(format!("dynamic length word at {off:#x} is out of bounds"));
                }
                let len = decode_job_id(&args[off..off + 32])
                    .map_err(|_| "dynamic length overflows".to_string())?
                    as usize;
                if off + 32 + len > args.len() {
                    return Err(format!("dynamic payload at {off:#x} overruns the calldata"));
                }
                Ok(len)
            };
            let result_len = read_len(off_result)?;
            let expected_proof_off = 0x60 + 32 + pad32(result_len);
            if off_proof != expected_proof_off {
                return Err(format!(
                    "non-canonical proof offset {off_proof:#x} (expected {expected_proof_off:#x})"
                ));
            }
            let proof_len = read_len(off_proof)?;
            let expected_total = off_proof + 32 + pad32(proof_len);
            if args.len() != expected_total {
                return Err(format!(
                    "submitResult calldata is {} bytes, canonical encoding is {expected_total}",
                    args.len()
                ));
            }
            Ok(DecodedArgs::Result {
                job_id,
                result_len,
                proof_len,
            })
        }
        "claimRewards" | "heartbeat" => {
            if !args.is_empty() {
                return Err(format!(
                    "{intent}() takes no arguments, got {} bytes",
                    args.len()
                ));
            }
            Ok(DecodedArgs::NoArgs)
        }
        "bidOnJob" => {
            if args.len() != 96 {
                return Err(format!(
                    "bidOnJob(uint256,uint256,uint256) takes exactly three words, got {} bytes",
                    args.len()
                ));
            }
            let job_id = decode_job_id(word(0))?;
            let price_wei = decode_u128_word(word(1), "bid price")?;
            let latency_ms = decode_u128_word(word(2), "estimated latency")?;
            // A bid at/over the Commitment cap can never be served by the
            // agent (the job auto-upgrades to the ZK tier) — not a plausible
            // agent bid, refuse it.
            if price_wei >= COMMITMENT_CAP_WEI {
                return Err(format!(
                    "bid price {price_wei} wei is at/over the 10 SALT Commitment cap"
                ));
            }
            Ok(DecodedArgs::Bid {
                job_id,
                price_wei,
                latency_ms,
            })
        }
        other => Err(format!("no argument shape known for intent {other:?}")),
    }
}

/// A request that passed every check — safe to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWrite {
    pub id: u64,
    pub intent: String,
    pub to: String,
    pub value_wei: String,
    pub calldata: Vec<u8>,
    /// Decoded arguments (FUA-GUI-01 residual) — what a confirmation
    /// surface shows the user.
    pub args: DecodedArgs,
}

impl ValidatedWrite {
    /// One-line human-readable description for confirmation surfaces/logs.
    pub fn describe(&self) -> String {
        let args = match &self.args {
            DecodedArgs::JobId { job_id } => format!("job {job_id}"),
            DecodedArgs::Commitment { job_id, commitment } => {
                format!(
                    "job {job_id}, commitment 0x{}…",
                    hex::encode(&commitment[..8])
                )
            }
            DecodedArgs::Result {
                job_id,
                result_len,
                proof_len,
            } => {
                format!("job {job_id}, result {result_len}B, proof {proof_len}B")
            }
            DecodedArgs::NoArgs => "no arguments".to_string(),
            DecodedArgs::Bid {
                job_id,
                price_wei,
                latency_ms,
            } => {
                format!("job {job_id}, price {price_wei} wei, latency {latency_ms}ms")
            }
        };
        format!(
            "{}({}) → {} [value {}]",
            self.intent, args, self.to, self.value_wei
        )
    }
}

/// FUA-GUI-01 residual: which writes need an explicit per-write human
/// confirmation. Pure decision function (unit-tested; the GUI wires the
/// answer into its approval surface):
///   - any value-bearing write (defense-in-depth — the validator refuses
///     non-zero values outright today), and
///   - `claimRewards` — it moves the provider's accrued rewards, unlike the
///     value-0 job-lifecycle writes covered by the relay's opt-in consent.
pub fn requires_confirmation(intent: &str, value_wei: &str) -> bool {
    value_wei != "0" || intent == "claimRewards"
}

/// Per-write human confirmation surface (FUA-GUI-01 residual). Implementors
/// must resolve `true` ONLY on an explicit user approval of this exact
/// write; timeout, rejection, or the absence of any surface is `false`.
#[async_trait::async_trait]
pub trait ConfirmationGate: Send + Sync {
    async fn confirm_write(&self, write: &ValidatedWrite) -> bool;
}

/// Fail-closed default gate: refuses every confirmation-requiring write.
/// Headless contexts that cannot ask a human use this.
pub struct DenyAllConfirmations;

#[async_trait::async_trait]
impl ConfirmationGate for DenyAllConfirmations {
    async fn confirm_write(&self, _write: &ValidatedWrite) -> bool {
        false
    }
}

/// Validates queued requests against the SELL allow-list before signing.
pub struct RelayValidator {
    /// The chain the wallet is signing for (40204).
    pub chain_id: u64,
    /// `ComputeMarketplace` address (job-lifecycle writes + `bidOnJob` target this).
    pub marketplace: String,
    /// `ContributionAccounting` address (`claimRewards` targets this).
    pub accounting: String,
    /// `HeartbeatMonitor` address (`heartbeat` targets this).
    pub heartbeat_monitor: String,
}

impl RelayValidator {
    pub fn new(
        chain_id: u64,
        marketplace: impl Into<String>,
        accounting: impl Into<String>,
        heartbeat_monitor: impl Into<String>,
    ) -> Self {
        Self {
            chain_id,
            marketplace: marketplace.into(),
            accounting: accounting.into(),
            heartbeat_monitor: heartbeat_monitor.into(),
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
            "bidOnJob" => (&self.marketplace, SEL_BID_ON_JOB),
            "heartbeat" => (&self.heartbeat_monitor, SEL_HEARTBEAT),
            other => return Err(RejectReason::UnknownIntent(other.to_string())),
        };
        if !addr_eq(&req.to, expected_contract) {
            return Err(RejectReason::WrongContract {
                intent: req.intent.clone(),
                to: req.to.clone(),
            });
        }
        let calldata = decode_hex(&req.calldata).map_err(RejectReason::BadCalldata)?;
        if calldata.len() < 4 || calldata[0..4] != expected_selector {
            return Err(RejectReason::SelectorMismatch {
                intent: req.intent.clone(),
            });
        }
        // FUA-GUI-01 residual (WP 6.4b): a matching selector is not enough —
        // the ARGUMENTS must decode to the intent's exact ABI shape too.
        if calldata.len() > MAX_CALLDATA_BYTES {
            return Err(RejectReason::MalformedArgs {
                intent: req.intent.clone(),
                reason: format!("calldata exceeds {MAX_CALLDATA_BYTES} bytes"),
            });
        }
        let args =
            decode_args(&req.intent, &calldata).map_err(|reason| RejectReason::MalformedArgs {
                intent: req.intent.clone(),
                reason,
            })?;
        Ok(ValidatedWrite {
            id: req.id,
            intent: req.intent.clone(),
            to: req.to.clone(),
            value_wei: req.value_wei.clone(),
            calldata,
            args,
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
/// Privileged/value-bearing writes additionally require `gate` to confirm them
/// (FUA-GUI-01 residual) — an unconfirmed write is recorded, never signed.
pub async fn run_once(
    signer: &dyn TxSigner,
    from: &str,
    queue: &dyn RequestQueue,
    validator: &RelayValidator,
    gate: &dyn ConfirmationGate,
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
                // FUA-GUI-01 residual: privileged writes need an explicit
                // per-write human confirmation; declined → refused, fail closed.
                if requires_confirmation(&vw.intent, &vw.value_wei)
                    && !gate.confirm_write(&vw).await
                {
                    report
                        .rejected
                        .push((req.id, RejectReason::NotConfirmed { intent: vw.intent }));
                    continue;
                }
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
                        report
                            .errors
                            .push(format!("sign {} ({}): {e}", vw.id, vw.intent));
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
    /// `HeartbeatMonitor` address (`heartbeat` liveness writes).
    pub heartbeat_monitor: String,
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
        RelayValidator::new(
            self.config.chain_id,
            &self.config.marketplace,
            &self.config.accounting,
            &self.config.heartbeat_monitor,
        )
    }

    /// Run one gated cycle. Returns `None` when the relay is disabled or the
    /// wallet is locked (nothing signed); otherwise the [`RelayTickReport`].
    /// `gate` confirms privileged writes (FUA-GUI-01 residual).
    pub async fn tick(
        &self,
        signer: &dyn TxSigner,
        from: &str,
        queue: &dyn RequestQueue,
        unlocked: bool,
        gate: &dyn ConfirmationGate,
    ) -> Option<RelayTickReport> {
        if !self.is_enabled() || !unlocked {
            return None;
        }
        Some(run_once(signer, from, queue, &self.validator(), gate).await)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Case-insensitive `0x`-hex address equality.
fn addr_eq(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        s.trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X")
            .to_lowercase()
    };
    n(a) == n(b)
}

/// Decode a `0x`-prefixed (or bare) hex string into bytes.
///
/// PBA-L7b-016: works on BYTES, never `&str` slices. The old `&h[i..i + 2]` panicked on a
/// non-ASCII char (a slice inside a multi-byte UTF-8 sequence), and the panic killed the signing
/// relay thread for good. Any non-hex byte is now an ordinary `Err` (the request is rejected).
fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let h = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let b = h.as_bytes();
    if !b.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    fn nib(c: u8) -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err("non-hex digit".to_string()),
        }
    }
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|p| Ok((nib(p[0])? << 4) | nib(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// PBA-L7b-016: non-ASCII calldata from node-agent must be rejected, not panic the relay.
    #[test]
    fn pba_l7b_016_decode_hex_rejects_non_ascii_without_panicking() {
        for evil in [
            "0xa\u{e9}b",
            "0x\u{e9}\u{e9}",
            "\u{1F600}\u{1F600}",
            "0xzz",
            "0xabc",
        ] {
            let r = std::panic::catch_unwind(|| decode_hex(evil));
            assert!(r.is_ok(), "decode_hex panicked on {evil:?}");
            assert!(r.unwrap().is_err(), "{evil:?} must be rejected");
        }
        assert_eq!(
            decode_hex("0xdeADbe01").unwrap(),
            vec![0xde, 0xad, 0xbe, 0x01]
        );
        assert_eq!(decode_hex(" 0X00ff ").unwrap(), vec![0x00, 0xff]);
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_hex("9aF0").unwrap(), vec![0x9a, 0xf0]);
    }

    const MARKETPLACE: &str = "0xc12dbcdb80ef2ae675315f455210f39a736a373c";
    const ACCOUNTING: &str = "0x86d918808b48ad543c9c816b5303b7dbcb0e321f";
    const HEARTBEAT_MONITOR: &str = "0xe9eaac272844f342266862bbefc6d117a227ad9b";
    const FROM: &str = "0xabababababababababababababababababababab";

    fn validator() -> RelayValidator {
        RelayValidator::new(40204, MARKETPLACE, ACCOUNTING, HEARTBEAT_MONITOR)
    }

    /// Canonical ABI arguments for the function a selector belongs to, so the
    /// shape-validating relay accepts the fixture (FUA-GUI-01 residual).
    fn canonical_args_for(selector: [u8; 4], job_id: u64) -> String {
        let word = |v: u64| format!("{:0>64x}", v);
        match selector {
            s if s == SEL_START_EXECUTION || s == SEL_COMPLETE_JOB => word(job_id),
            s if s == SEL_SUBMIT_COMMITMENT => format!("{}{}", word(job_id), "11".repeat(32)),
            s if s == SEL_SUBMIT_RESULT => {
                // submitResult(jobId, bytes result, bytes proof) — 3-byte
                // result, 2-byte proof, canonical offsets.
                let mut a = String::new();
                a.push_str(&word(job_id));
                a.push_str(&word(0x60)); // result offset
                a.push_str(&word(0x60 + 32 + 32)); // proof offset (past padded result)
                a.push_str(&word(3)); // result length
                a.push_str(&format!("{:0<64}", "aabbcc")); // 3 bytes, right-padded
                a.push_str(&word(2)); // proof length
                a.push_str(&format!("{:0<64}", "ddee")); // 2 bytes, right-padded
                a
            }
            s if s == SEL_CLAIM_REWARDS || s == SEL_HEARTBEAT => String::new(),
            s if s == SEL_BID_ON_JOB => {
                // bidOnJob(jobId, price 1 SALT — under the Commitment cap, latency 600s).
                format!(
                    "{}{}{}",
                    word(job_id),
                    word(1_000_000_000_000_000_000),
                    word(600_000)
                )
            }
            _ => word(job_id), // unknown selectors: one plausible word
        }
    }

    /// A pending request with a given intent, contract, value, and selector.
    fn req(id: u64, intent: &str, to: &str, value_wei: &str, selector: [u8; 4]) -> PendingRequest {
        let mut calldata = format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            selector[0], selector[1], selector[2], selector[3]
        );
        calldata.push_str(&canonical_args_for(selector, id));
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

    // FUA-GUI-02: the relay must only talk to a loopback node-agent.
    #[test]
    fn validate_node_agent_url_accepts_loopback() {
        assert!(validate_node_agent_url("http://127.0.0.1:19600").is_ok());
        assert!(validate_node_agent_url("http://localhost:19600").is_ok());
        assert!(validate_node_agent_url("http://[::1]:19600").is_ok());
        assert!(validate_node_agent_url("http://127.0.0.1:25000/").is_ok());
    }

    #[test]
    fn validate_node_agent_url_rejects_remote_and_non_http() {
        // A routable host — confused-deputy / relay pointed at someone else's node.
        assert!(validate_node_agent_url("http://10.0.0.5:19600").is_err());
        assert!(validate_node_agent_url("http://evil.example:19600").is_err());
        assert!(validate_node_agent_url("http://0.0.0.0:19600").is_err());
        // Non-http scheme (the loopback queue is plaintext-local).
        assert!(validate_node_agent_url("https://evil.example").is_err());
        assert!(validate_node_agent_url("ftp://127.0.0.1").is_err());
    }

    // NAT-B-008: the userinfo confused-deputy vectors. A loopback host
    // followed by `@realhost` MUST be rejected — reqwest resolves the host
    // AFTER the `@`, so the pre-fix `rsplit_once(':')` waved through a URL
    // that actually talks to `evil.example`.
    #[test]
    fn validate_node_agent_url_rejects_userinfo_bypass() {
        assert!(validate_node_agent_url("http://127.0.0.1:19600@evil.example/").is_err());
        assert!(validate_node_agent_url("http://localhost:1@attacker.tld/").is_err());
        assert!(validate_node_agent_url("http://127.0.0.1@evil.example").is_err());
        assert!(validate_node_agent_url("http://[::1]@evil.example").is_err());
        // `#@evil` is a fragment and `\@evil` normalizes to a path under
        // WHATWG parsing — the resolved host stays 127.0.0.1, so these are
        // genuinely safe and must NOT be false-rejected.
        assert!(validate_node_agent_url("http://127.0.0.1#@evil").is_ok());
        assert!(validate_node_agent_url("http://127.0.0.1\\@evil").is_ok());
    }

    #[test]
    fn deserializes_node_agent_queue_json() {
        let json = r#"[
          {"id":0,"intent":"submitResult","to":"0xc12DBCDB80Ef2aE675315F455210f39a736a373c",
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
            ("bidOnJob", MARKETPLACE, SEL_BID_ON_JOB),
            ("heartbeat", HEARTBEAT_MONITOR, SEL_HEARTBEAT),
        ] {
            let r = req(1, intent, to, "0", sel);
            let vw = v
                .validate(&r)
                .unwrap_or_else(|e| panic!("{intent} should pass: {e:?}"));
            assert_eq!(vw.intent, intent);
            assert_eq!(&vw.calldata[0..4], &sel);
        }
    }

    /// SELL-S1 additions: the bid decodes to its exact shape; the cap holds.
    #[test]
    fn validator_decodes_bid_args_and_refuses_cap_breach() {
        let v = validator();
        let r = req(7, "bidOnJob", MARKETPLACE, "0", SEL_BID_ON_JOB);
        let vw = v.validate(&r).expect("canonical bid passes");
        assert_eq!(
            vw.args,
            DecodedArgs::Bid {
                job_id: 7,
                price_wei: 1_000_000_000_000_000_000,
                latency_ms: 600_000
            }
        );

        // A bid at the 10 SALT Commitment cap is never a legitimate agent bid
        // (the job auto-upgrades to the ZK tier) — refuse to sign it.
        let word = |v: u128| format!("{v:0>64x}");
        let mut r = req(7, "bidOnJob", MARKETPLACE, "0", SEL_BID_ON_JOB);
        r.calldata = format!(
            "0x18360fc2{}{}{}",
            word(7),
            word(10_000_000_000_000_000_000), // exactly 10 SALT
            word(600_000)
        );
        assert!(
            matches!(v.validate(&r), Err(RejectReason::MalformedArgs { .. })),
            "10 SALT bid must be refused"
        );
    }

    #[test]
    fn validator_refuses_heartbeat_with_args_or_wrong_contract() {
        let v = validator();
        // heartbeat() with smuggled argument bytes → refused.
        let mut r = req(1, "heartbeat", HEARTBEAT_MONITOR, "0", SEL_HEARTBEAT);
        r.calldata.push_str(&"00".repeat(32));
        assert!(matches!(
            v.validate(&r),
            Err(RejectReason::MalformedArgs { .. })
        ));
        // heartbeat aimed at the marketplace → refused.
        let r2 = req(1, "heartbeat", MARKETPLACE, "0", SEL_HEARTBEAT);
        assert!(matches!(
            v.validate(&r2),
            Err(RejectReason::WrongContract { .. })
        ));
        // bidOnJob aimed at the heartbeat monitor → refused.
        let r3 = req(1, "bidOnJob", HEARTBEAT_MONITOR, "0", SEL_BID_ON_JOB);
        assert!(matches!(
            v.validate(&r3),
            Err(RejectReason::WrongContract { .. })
        ));
    }

    #[test]
    fn validator_accepts_mixed_case_address() {
        let v = validator();
        let r = req(
            1,
            "submitResult",
            "0xc12DBCDB80Ef2aE675315F455210f39a736a373c",
            "0",
            SEL_SUBMIT_RESULT,
        );
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
        assert_eq!(
            v.validate(&r),
            Err(RejectReason::WrongChain {
                expected: 40204,
                got: 1
            })
        );
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
        assert_eq!(
            v.validate(&r),
            Err(RejectReason::UnknownIntent("drainTreasury".into()))
        );
    }

    #[test]
    fn validator_refuses_wrong_contract() {
        let v = validator();
        // claimRewards must target accounting, not the marketplace.
        let r = req(1, "claimRewards", MARKETPLACE, "0", SEL_CLAIM_REWARDS);
        assert!(matches!(
            v.validate(&r),
            Err(RejectReason::WrongContract { .. })
        ));
        // an attacker-supplied 'to' is refused.
        let r2 = req(
            1,
            "submitResult",
            "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "0",
            SEL_SUBMIT_RESULT,
        );
        assert!(matches!(
            v.validate(&r2),
            Err(RejectReason::WrongContract { .. })
        ));
    }

    #[test]
    fn validator_refuses_selector_mismatch() {
        let v = validator();
        // intent says submitResult but the calldata carries completeJob's selector.
        let r = req(1, "submitResult", MARKETPLACE, "0", SEL_COMPLETE_JOB);
        assert_eq!(
            v.validate(&r),
            Err(RejectReason::SelectorMismatch {
                intent: "submitResult".into()
            })
        );
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
            Self {
                requests,
                observed: Mutex::new(Vec::new()),
                fail_observe: false,
            }
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
            self.observed
                .lock()
                .unwrap()
                .push((id, tx_hash.to_string()));
            Ok(())
        }
    }

    struct MockSigner {
        ok: bool,
        calls: Mutex<Vec<(String, String, String, Vec<u8>)>>,
    }
    impl MockSigner {
        fn new(ok: bool) -> Self {
            Self {
                ok,
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl TxSigner for MockSigner {
        async fn sign_and_send(
            &self,
            from: &str,
            to: &str,
            value_wei: &str,
            data: Vec<u8>,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((from.into(), to.into(), value_wei.into(), data));
            if self.ok {
                Ok("0xdeadbeef".to_string())
            } else {
                Err("broadcast failed: node down".to_string())
            }
        }
    }

    /// Records which writes it was asked to confirm; answers `allow`.
    struct MockGate {
        allow: bool,
        asked: Mutex<Vec<u64>>,
    }
    impl MockGate {
        fn new(allow: bool) -> Self {
            Self {
                allow,
                asked: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl ConfirmationGate for MockGate {
        async fn confirm_write(&self, write: &ValidatedWrite) -> bool {
            self.asked.lock().unwrap().push(write.id);
            self.allow
        }
    }

    #[tokio::test]
    async fn run_once_signs_validates_and_observes() {
        let q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator(), &MockGate::new(true)).await;

        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 7);
        assert_eq!(report.signed[0].tx_hash, "0xdeadbeef");
        // It signed against the right account + contract + value.
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls[0].0, FROM);
        assert_eq!(calls[0].2, "0");
        // It POSTed observed with the hash.
        assert_eq!(
            q.observed.lock().unwrap().as_slice(),
            &[(7, "0xdeadbeef".to_string())]
        );
    }

    #[tokio::test]
    async fn run_once_broadcast_failure_leaves_it_pending() {
        let q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s = MockSigner::new(false); // broadcast fails
        let report = run_once(&s, FROM, &q, &validator(), &MockGate::new(true)).await;

        assert!(report.signed.is_empty());
        assert_eq!(report.errors.len(), 1);
        // Crucially: no observed POST without a real hash.
        assert!(q.observed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_once_skips_submitted_and_refuses_bad_then_signs_valid() {
        let mut submitted = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        submitted.status = "submitted".into();
        let bad_to = req(
            2,
            "submitResult",
            "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "0",
            SEL_SUBMIT_RESULT,
        );
        let good = req(3, "claimRewards", ACCOUNTING, "0", SEL_CLAIM_REWARDS);
        let q = MockQueue::new(vec![submitted, bad_to, good]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator(), &MockGate::new(true)).await;

        assert_eq!(report.skipped_non_pending, 1);
        assert_eq!(report.rejected.len(), 1);
        assert!(matches!(
            report.rejected[0],
            (2, RejectReason::WrongContract { .. })
        ));
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
        let report = run_once(&s, FROM, &q, &validator(), &MockGate::new(true)).await;
        // Only the first valid write is signed (nonce safety); #2 waits for next tick.
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 1);
        assert_eq!(s.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_once_records_observe_failure_but_still_reports_signed() {
        let mut q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        q.fail_observe = true;
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator(), &MockGate::new(true)).await;
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
            heartbeat_monitor: HEARTBEAT_MONITOR.into(),
            poll_interval: std::time::Duration::from_secs(5),
        })
    }

    #[tokio::test]
    async fn service_tick_is_noop_when_disabled() {
        let svc = relay_service(); // disabled by default (opt-in)
        assert!(!svc.is_enabled());
        let q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s = MockSigner::new(true);
        // Even unlocked, a disabled relay signs nothing.
        assert!(svc
            .tick(&s, FROM, &q, true, &MockGate::new(true))
            .await
            .is_none());
        assert!(s.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_tick_is_noop_when_locked() {
        let svc = relay_service();
        svc.set_enabled(true);
        let q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s = MockSigner::new(true);
        // Enabled but locked → nothing signed (never auto-unlocks).
        assert!(svc
            .tick(&s, FROM, &q, false, &MockGate::new(true))
            .await
            .is_none());
        assert!(s.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_tick_runs_when_enabled_and_unlocked() {
        let svc = relay_service();
        svc.set_enabled(true);
        let q = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s = MockSigner::new(true);
        let report = svc
            .tick(&s, FROM, &q, true, &MockGate::new(true))
            .await
            .expect("should run");
        assert_eq!(report.signed.len(), 1);
        assert_eq!(report.signed[0].id, 7);
    }

    // ── FUA-GUI-01 residual (WP 6.4b): calldata ARGUMENTS must match the
    // intent's ABI shape, not just its 4-byte selector. Each of these was
    // accepted by the selector-only validator — RED until decode_args lands.

    /// `claimRewards()` takes NO arguments. Trailing words are refused.
    #[test]
    fn validator_refuses_claim_rewards_with_unexpected_args() {
        let v = validator();
        let mut r = req(1, "claimRewards", ACCOUNTING, "0", SEL_CLAIM_REWARDS);
        r.calldata = format!("0x{}{}", hex::encode(SEL_CLAIM_REWARDS), "00".repeat(32));
        assert!(
            v.validate(&r).is_err(),
            "claimRewards with argument words must be refused"
        );
    }

    /// `startExecution(uint256)` is exactly selector + one 32-byte word.
    #[test]
    fn validator_refuses_start_execution_with_trailing_bytes() {
        let v = validator();
        let mut r = req(1, "startExecution", MARKETPLACE, "0", SEL_START_EXECUTION);
        r.calldata = format!(
            "0x{}{}ff",
            hex::encode(SEL_START_EXECUTION),
            "00".repeat(32)
        );
        assert!(
            v.validate(&r).is_err(),
            "startExecution with a trailing byte must be refused"
        );
    }

    /// `submitCommitment(uint256,bytes32)` is exactly selector + two words.
    #[test]
    fn validator_refuses_truncated_submit_commitment() {
        let v = validator();
        let mut r = req(
            1,
            "submitCommitment",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_COMMITMENT,
        );
        r.calldata = format!(
            "0x{}{}",
            hex::encode(SEL_SUBMIT_COMMITMENT),
            "00".repeat(32)
        );
        assert!(
            v.validate(&r).is_err(),
            "submitCommitment with only one argument word must be refused"
        );
    }

    /// `submitResult(uint256,bytes,bytes)` head offsets must be canonical and
    /// in-bounds — an attacker-crafted head pointing past the calldata is refused.
    #[test]
    fn validator_refuses_submit_result_with_bogus_offsets() {
        let v = validator();
        let mut r = req(1, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        // jobId=1, then two offsets pointing far outside the calldata.
        let mut args = String::new();
        args.push_str(&format!("{:0>64x}", 1u64)); // jobId
        args.push_str(&format!("{:0>64x}", 0xffff_u64)); // result offset (way out)
        args.push_str(&format!("{:0>64x}", 0xffff_u64)); // proof offset (way out)
        r.calldata = format!("0x{}{}", hex::encode(SEL_SUBMIT_RESULT), args);
        assert!(
            v.validate(&r).is_err(),
            "submitResult with out-of-bounds dynamic offsets must be refused"
        );
    }

    /// A jobId with non-zero high bytes (> u128) is not a plausible job
    /// counter — refuse it rather than sign an arbitrary 256-bit value.
    #[test]
    fn validator_refuses_implausible_job_id() {
        let v = validator();
        let mut r = req(1, "completeJob", MARKETPLACE, "0", SEL_COMPLETE_JOB);
        r.calldata = format!("0x{}{}", hex::encode(SEL_COMPLETE_JOB), "ff".repeat(32));
        assert!(
            v.validate(&r).is_err(),
            "completeJob with a 2^255-scale jobId must be refused"
        );
    }

    /// Decoded args surface what the relay is signing (display + cross-check).
    #[test]
    fn decode_args_extracts_each_shape() {
        let v = validator();
        let r = req(
            9,
            "submitCommitment",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_COMMITMENT,
        );
        let vw = v.validate(&r).expect("valid");
        assert_eq!(
            vw.args,
            DecodedArgs::Commitment {
                job_id: 9,
                commitment: [0x11; 32]
            }
        );

        let r = req(4, "submitResult", MARKETPLACE, "0", SEL_SUBMIT_RESULT);
        let vw = v.validate(&r).expect("valid");
        assert_eq!(
            vw.args,
            DecodedArgs::Result {
                job_id: 4,
                result_len: 3,
                proof_len: 2
            }
        );
        assert!(vw.describe().contains("job 4"));

        let r = req(2, "claimRewards", ACCOUNTING, "0", SEL_CLAIM_REWARDS);
        assert_eq!(v.validate(&r).expect("valid").args, DecodedArgs::NoArgs);
    }

    // ── FUA-GUI-01 residual: per-write confirmation gate ──────────────────

    /// The pure decision function: claimRewards and any value-bearing write
    /// require explicit confirmation; value-0 job lifecycle writes do not.
    #[test]
    fn requires_confirmation_policy() {
        assert!(requires_confirmation("claimRewards", "0"));
        assert!(requires_confirmation("submitResult", "5"));
        assert!(requires_confirmation(
            "anythingValueBearing",
            "1000000000000000000"
        ));
        assert!(!requires_confirmation("startExecution", "0"));
        assert!(!requires_confirmation("submitCommitment", "0"));
        assert!(!requires_confirmation("submitResult", "0"));
        assert!(!requires_confirmation("completeJob", "0"));
    }

    /// An unconfirmed claimRewards is refused — and never signed.
    #[tokio::test]
    async fn run_once_refuses_unconfirmed_claim_rewards() {
        let q = MockQueue::new(vec![req(
            5,
            "claimRewards",
            ACCOUNTING,
            "0",
            SEL_CLAIM_REWARDS,
        )]);
        let s = MockSigner::new(true);
        let gate = MockGate::new(false); // user declines / no surface
        let report = run_once(&s, FROM, &q, &validator(), &gate).await;

        assert!(
            report.signed.is_empty(),
            "declined write must not be signed"
        );
        assert!(
            s.calls.lock().unwrap().is_empty(),
            "signer must never be reached"
        );
        assert_eq!(
            gate.asked.lock().unwrap().as_slice(),
            &[5],
            "gate was consulted"
        );
        assert!(matches!(
            report.rejected.as_slice(),
            [(5, RejectReason::NotConfirmed { .. })]
        ));
        assert!(
            q.observed.lock().unwrap().is_empty(),
            "no observed POST either"
        );
    }

    /// A confirmed claimRewards signs; value-0 lifecycle writes never consult
    /// the gate (covered by the relay's explicit opt-in consent).
    #[tokio::test]
    async fn run_once_confirmed_claim_rewards_signs_and_lifecycle_skips_gate() {
        let q = MockQueue::new(vec![req(
            5,
            "claimRewards",
            ACCOUNTING,
            "0",
            SEL_CLAIM_REWARDS,
        )]);
        let s = MockSigner::new(true);
        let gate = MockGate::new(true);
        let report = run_once(&s, FROM, &q, &validator(), &gate).await;
        assert_eq!(report.signed.len(), 1);
        assert_eq!(gate.asked.lock().unwrap().as_slice(), &[5]);

        let q2 = MockQueue::new(vec![req(
            7,
            "submitResult",
            MARKETPLACE,
            "0",
            SEL_SUBMIT_RESULT,
        )]);
        let s2 = MockSigner::new(true);
        let gate2 = MockGate::new(false); // even a denying gate is irrelevant here
        let report2 = run_once(&s2, FROM, &q2, &validator(), &gate2).await;
        assert_eq!(
            report2.signed.len(),
            1,
            "value-0 lifecycle write signs without the gate"
        );
        assert!(
            gate2.asked.lock().unwrap().is_empty(),
            "gate not consulted for lifecycle writes"
        );
    }

    /// The fail-closed default gate refuses everything.
    #[tokio::test]
    async fn deny_all_gate_refuses() {
        let q = MockQueue::new(vec![req(
            5,
            "claimRewards",
            ACCOUNTING,
            "0",
            SEL_CLAIM_REWARDS,
        )]);
        let s = MockSigner::new(true);
        let report = run_once(&s, FROM, &q, &validator(), &DenyAllConfirmations).await;
        assert!(report.signed.is_empty());
        assert!(matches!(
            report.rejected.as_slice(),
            [(5, RejectReason::NotConfirmed { .. })]
        ));
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
        let report = run_once(&s, FROM, &DeadQueue, &validator(), &MockGate::new(true)).await;
        assert!(report.signed.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("list requests"));
    }
}
