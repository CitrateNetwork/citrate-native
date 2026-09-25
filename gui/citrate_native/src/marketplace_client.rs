//! ComputeMarketplace + ContributionAccounting client — P960-D WP-D.3/D.4.
//!
//! Hand-rolled ABI encode/decode + eth_call wrapper so we don't take
//! a hard dependency on ethers or alloy. Every function here names the
//! exact on-chain function it wraps (Rule 11: data source tracing).
//!
//! - `registerProvider(bytes32[])` payable — `ComputeMarketplace.sol:273`
//! - `getProvider(address)` view       — `ComputeMarketplace.sol:829`
//! - `claimable(address)` view         — `ContributionAccounting.sol:47` (public mapping)
//! - `claimRewards()` nonpayable       — `ContributionAccounting.sol:188`
//! - `nextPoolId()` view               — `LearningPool.sol:35` (public counter)
//! - `isMember(uint256,address)` view  — `LearningPool.sol:37` (public mapping)
//! - `stakes(uint256,address)` view    — `LearningPool.sol:38` (public mapping)
//! - `joinPool(uint256)` payable       — `LearningPool.sol:120`
//! - `leavePool(uint256)` nonpayable   — `LearningPool.sol:153`
//!
//! We use `claimable(address)` (settled, post-distribution balance)
//! rather than `pendingReward(address)` (pre-distribution estimate).
//! Claimable is the authoritative "you can withdraw this now" number;
//! the UI should show *certain* earnings, not estimates.

use sha3::{Digest, Keccak256};

// ── Canonical address-table loader ───────────────────────────────────
//
// Source of truth: the federation-canonical contract-address table
// vendored at `src/generated/addresses.json`
// (which mirrors `citrate-chain/contracts/addresses/40204.json`).
// After a chain re-roll + post-redeploy ceremony, run
// `bash scripts/sync-addresses.sh` from the gui-native repo root to
// re-vendor the table — no inline edit, no chance of drift versus the
// gateway / node-agent / explorer.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Vendored copy of the federation-canonical contract-address table.
const ADDRESS_TABLE_JSON: &str = include_str!("generated/addresses.json");

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalTable {
    chain_id: u64,
    contracts: HashMap<String, String>,
    aa_stack: HashMap<String, String>,
}

static CANONICAL: LazyLock<CanonicalTable> = LazyLock::new(|| {
    serde_json::from_str::<CanonicalTable>(ADDRESS_TABLE_JSON)
        .expect("src/generated/addresses.json is malformed at build time")
});

/// Flat name → 'static-leaked address map combining `contracts` +
/// `aaStack`. Box::leak-ed so the public API can return `&'static str`.
static NAME_TO_ADDRESS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut book = HashMap::with_capacity(CANONICAL.contracts.len() + CANONICAL.aa_stack.len());
    for (name, addr) in CANONICAL.contracts.iter().chain(CANONICAL.aa_stack.iter()) {
        let n: &'static str = Box::leak(name.clone().into_boxed_str());
        let a: &'static str = Box::leak(addr.clone().into_boxed_str());
        book.insert(n, a);
    }
    book
});

/// Sorted list of every contract name in the canonical table, leaked
/// to 'static so the public API stays zero-allocation per call.
static NAMES_SORTED: LazyLock<Box<[&'static str]>> = LazyLock::new(|| {
    let mut names: Vec<&'static str> = NAME_TO_ADDRESS.keys().copied().collect();
    names.sort_unstable();
    names.into_boxed_slice()
});

fn canonical_lookup(name: &str) -> Option<&'static str> {
    NAME_TO_ADDRESS.get(name).copied()
}

/// Known marketplace deployment by chain ID. Reads from the vendored
/// canonical table. Unknown chain IDs return `None` — the UI surfaces
/// that as "Marketplace not deployed on this network" rather than
/// silently pointing at a zero address.
pub fn compute_marketplace_address(chain_id: u64) -> Option<&'static str> {
    if chain_id != CANONICAL.chain_id {
        return None;
    }
    canonical_lookup("ComputeMarketplace")
}

pub fn contribution_accounting_address(chain_id: u64) -> Option<&'static str> {
    if chain_id != CANONICAL.chain_id {
        return None;
    }
    canonical_lookup("ContributionAccounting")
}

pub fn learning_pool_address(chain_id: u64) -> Option<&'static str> {
    if chain_id != CANONICAL.chain_id {
        return None;
    }
    canonical_lookup("LearningPool")
}

pub fn model_registry_address(chain_id: u64) -> Option<&'static str> {
    if chain_id != CANONICAL.chain_id {
        return None;
    }
    canonical_lookup("ModelRegistry")
}

/// Full address book for the canonical chain (40204). T2-7 — keeps the
/// GUI from being a stranger to the rest of the deployed contracts.
/// Future panels and chat tools resolve names via this map instead of
/// hardcoding addresses one-by-one.
#[allow(dead_code)] // Used by future panels + chat tools (T2-7 forward infra)
pub fn known_contract(chain_id: u64, name: &str) -> Option<&'static str> {
    if chain_id != CANONICAL.chain_id {
        return None;
    }
    canonical_lookup(name)
}

/// All deployed contract names known on this chain. Used by the
/// chat agent and any panel that wants to enumerate the address
/// book ("what contracts are out there?").
#[allow(dead_code)] // Used by future panels + chat tools (T2-7 forward infra)
pub fn known_contract_names(chain_id: u64) -> &'static [&'static str] {
    if chain_id != CANONICAL.chain_id {
        return &[];
    }
    &NAMES_SORTED
}

// ── ModelRegistry ABI helpers (T2-6) ─────────────────────────────

/// Encode `getModelCount() → uint256`. ModelRegistry exposes the
/// total number of registered models so the GUI can paginate.
pub fn encode_model_count() -> Vec<u8> {
    selector("getModelCount()").to_vec()
}

/// Encode `getModelAt(uint256 index) → bytes32 modelId`. Returns
/// the ID of the Nth model in registration order.
#[allow(dead_code)] // Used by future ModelRegistry browser (T2-6 forward infra)
pub fn encode_get_model_at(index: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("getModelAt(uint256)"));
    let mut idx_word = [0u8; 32];
    idx_word[24..].copy_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&idx_word);
    out
}

/// Decode a 32-byte bytes32 result into a hex string with 0x prefix.
#[allow(dead_code)] // Used by future ModelRegistry browser (T2-6 forward infra)
pub fn decode_bytes32(hex_result: &str) -> Option<String> {
    let s = hex_result.strip_prefix("0x").unwrap_or(hex_result);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(format!("0x{}", hex::encode(bytes)))
}

/// Solidity's `MIN_PROVIDER_STAKE = 1000 ether` in wei.
pub const MIN_PROVIDER_STAKE_WEI: u128 = 1_000_000_000_000_000_000_000; // 1000 * 1e18

/// A canonical "supports any model" sentinel used for v1 registration.
/// `keccak256("any")` — well-known, deterministic, derivable off-chain.
/// Future UI will let users enumerate specific model bytes32 IDs.
pub fn any_model_hash() -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"any");
    h.finalize().into()
}

/// Parsed ProviderProfile struct. Layout matches ComputeMarketplace.sol:78.
/// `max_concurrent_jobs` is populated for completeness even though the
/// current UI only surfaces jobs-in-flight — future diagnostics may want it.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ProviderProfile {
    pub is_registered: bool,
    pub stake_wei: u128,
    pub total_jobs_completed: u64,
    pub total_jobs_failed: u64,
    /// Basis points; 10000 = 100%.
    pub reputation_bps: u64,
    pub current_active_jobs: u64,
    pub max_concurrent_jobs: u64,
}

fn selector(sig: &str) -> [u8; 4] {
    let mut h = Keccak256::new();
    h.update(sig.as_bytes());
    let out = h.finalize();
    [out[0], out[1], out[2], out[3]]
}

fn encode_address_padded(addr: &str) -> Option<[u8; 32]> {
    let s = addr.strip_prefix("0x").unwrap_or(addr);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&bytes);
    Some(out)
}

/// Encode `registerProvider(bytes32[] supportedModels)` call data.
/// Returns the 4-byte selector + ABI-encoded dynamic array.
pub fn encode_register_provider(models: &[[u8; 32]]) -> Vec<u8> {
    let sel = selector("registerProvider(bytes32[])");
    let mut out = Vec::with_capacity(4 + 64 + 32 * models.len());
    out.extend_from_slice(&sel);
    // Offset to the dynamic array data: 0x20 (one word after the head).
    let mut offset = [0u8; 32];
    offset[31] = 0x20;
    out.extend_from_slice(&offset);
    // Array length.
    let mut len = [0u8; 32];
    len[24..].copy_from_slice(&(models.len() as u64).to_be_bytes());
    out.extend_from_slice(&len);
    // Elements (already bytes32).
    for m in models {
        out.extend_from_slice(m);
    }
    out
}

pub fn encode_get_provider(addr: &str) -> Option<Vec<u8>> {
    let padded = encode_address_padded(addr)?;
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("getProvider(address)"));
    out.extend_from_slice(&padded);
    Some(out)
}

pub fn encode_claimable(addr: &str) -> Option<Vec<u8>> {
    let padded = encode_address_padded(addr)?;
    let mut out = Vec::with_capacity(36);
    // Public mapping `claimable` auto-generates `claimable(address)`.
    out.extend_from_slice(&selector("claimable(address)"));
    out.extend_from_slice(&padded);
    Some(out)
}

pub fn encode_claim_rewards() -> Vec<u8> {
    selector("claimRewards()").to_vec()
}

// ── LearningPool ABI helpers ─────────────────────────────────────────

pub fn encode_next_pool_id() -> Vec<u8> {
    // Public counter `nextPoolId` auto-generates `nextPoolId()`.
    selector("nextPoolId()").to_vec()
}

/// Encode `isMember(uint256 poolId, address user) → bool`.
pub fn encode_is_member(pool_id: u64, addr: &str) -> Option<Vec<u8>> {
    let padded_addr = encode_address_padded(addr)?;
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&selector("isMember(uint256,address)"));
    let mut id_word = [0u8; 32];
    id_word[24..].copy_from_slice(&pool_id.to_be_bytes());
    out.extend_from_slice(&id_word);
    out.extend_from_slice(&padded_addr);
    Some(out)
}

/// Encode `stakes(uint256 poolId, address user) → uint256`.
pub fn encode_stakes(pool_id: u64, addr: &str) -> Option<Vec<u8>> {
    let padded_addr = encode_address_padded(addr)?;
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&selector("stakes(uint256,address)"));
    let mut id_word = [0u8; 32];
    id_word[24..].copy_from_slice(&pool_id.to_be_bytes());
    out.extend_from_slice(&id_word);
    out.extend_from_slice(&padded_addr);
    Some(out)
}

/// T2-8: Parsed Pool struct from `getPool(uint256)` return.
/// Maps directly to `LearningPool.Pool` in the Solidity source.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // consumed by the Learning panel hydration
pub struct PoolInfo {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub creator: String, // 0x-prefixed EIP-55 checksum-less hex
    pub state: u8,       // 0=Active, 1=Closed, 2=ActiveCycle
    pub access: u8,      // 0=Open, 1=InviteOnly, 2=ApplicationRequired
    pub min_stake_wei: u128,
    pub member_count: u64,
    pub created_at: u64, // unix seconds
}

/// Encode `getPool(uint256 poolId) → Pool` view call.
pub fn encode_get_pool(pool_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("getPool(uint256)"));
    let mut id_word = [0u8; 32];
    id_word[24..].copy_from_slice(&pool_id.to_be_bytes());
    out.extend_from_slice(&id_word);
    out
}

/// Decode a `getPool()` return — a single Pool struct. Solidity
/// encodes this as an offset pointing at the tuple, followed by
/// the tuple contents (9 head words, then the two string tails).
///
/// Layout after the 0x20 outer offset:
///   slot 0  id              (uint256)
///   slot 1  offset_to_name  (uint256, relative to tuple start)
///   slot 2  offset_to_desc  (uint256, relative to tuple start)
///   slot 3  creator         (address, 20 bytes right-aligned)
///   slot 4  state           (uint8)
///   slot 5  access          (uint8)
///   slot 6  minStake        (uint256)
///   slot 7  memberCount     (uint256)
///   slot 8  createdAt       (uint256)
///   tail    name length + data (padded to 32)
///   tail    desc length + data (padded to 32)
pub fn decode_pool_info(hex_result: &str) -> Option<PoolInfo> {
    let s = hex_result.strip_prefix("0x").unwrap_or(hex_result);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() < 32 + 32 * 9 {
        return None;
    }
    // Outer offset — should be 0x20. We tolerate anything and just
    // use it as the tuple start.
    let tuple_start = u64::from_be_bytes(bytes[24..32].try_into().ok()?) as usize;
    if bytes.len() < tuple_start + 32 * 9 {
        return None;
    }

    // Head reads
    let slot = |i: usize| -> &[u8] { &bytes[tuple_start + i * 32..tuple_start + (i + 1) * 32] };
    let read_u64 = |i: usize| -> u64 {
        let s = slot(i);
        u64::from_be_bytes(s[24..32].try_into().unwrap_or([0u8; 8]))
    };
    let read_u128 = |i: usize| -> u128 {
        let s = slot(i);
        u128::from_be_bytes(s[16..32].try_into().unwrap_or([0u8; 16]))
    };
    let read_u8 = |i: usize| -> u8 { slot(i)[31] };
    let read_address = |i: usize| -> String {
        let s = slot(i);
        format!("0x{}", hex::encode(&s[12..32]))
    };

    let id = read_u64(0);
    let name_offset = u64::from_be_bytes(slot(1)[24..32].try_into().ok()?) as usize;
    let desc_offset = u64::from_be_bytes(slot(2)[24..32].try_into().ok()?) as usize;
    let creator = read_address(3);
    let state = read_u8(4);
    let access = read_u8(5);
    let min_stake_wei = read_u128(6);
    let member_count = read_u64(7);
    let created_at = read_u64(8);

    // Helper: read a length-prefixed UTF-8 string at `tuple_start + offset`
    let read_string = |offset: usize| -> Option<String> {
        let pos = tuple_start + offset;
        if bytes.len() < pos + 32 {
            return None;
        }
        let len = u64::from_be_bytes(bytes[pos + 24..pos + 32].try_into().ok()?) as usize;
        if bytes.len() < pos + 32 + len {
            return None;
        }
        let raw = &bytes[pos + 32..pos + 32 + len];
        // UTF-8 decode — lossy fallback so a broken string doesn't nuke
        // the whole row.
        Some(String::from_utf8_lossy(raw).into_owned())
    };
    let name = read_string(name_offset)?;
    let description = read_string(desc_offset)?;

    Some(PoolInfo {
        id,
        name,
        description,
        creator,
        state,
        access,
        min_stake_wei,
        member_count,
        created_at,
    })
}

/// Encode `createPool(string name, string description, uint8 access, uint256 minStake)`
/// payable. Returns the bytes32 pool id but the GUI doesn't need it
/// (the polling loop refreshes the list and finds the new one).
///
/// Solidity dynamic-type layout: 4-byte selector, then 4 head words
/// (two offsets + two static), then each string as `length || data`
/// padded to 32 bytes. The two offsets point past the head into the
/// tail. Access values: 0 = Open, 1 = InviteOnly, 2 = ApplicationRequired.
/// T2-9 — operators can seed pools from the GUI without `cast send`.
pub fn encode_create_pool(
    name: &str,
    description: &str,
    access: u8,
    min_stake_wei: u128,
) -> Vec<u8> {
    let sel = selector("createPool(string,string,uint8,uint256)");
    let name_bytes = name.as_bytes();
    let desc_bytes = description.as_bytes();
    let name_padded_len = name_bytes.len().div_ceil(32) * 32;
    let desc_padded_len = desc_bytes.len().div_ceil(32) * 32;
    // Offsets are measured from the start of the parameter section
    // (just after the 4-byte selector). Head is 4 × 32 = 128 bytes.
    let head_size: u64 = 4 * 32;
    let name_offset: u64 = head_size;
    let desc_offset: u64 = head_size + 32 + name_padded_len as u64;

    let mut out =
        Vec::with_capacity(4 + head_size as usize + 32 + name_padded_len + 32 + desc_padded_len);
    out.extend_from_slice(&sel);

    // Head slot 0: name offset
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&name_offset.to_be_bytes());
    out.extend_from_slice(&word);
    // Head slot 1: description offset
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&desc_offset.to_be_bytes());
    out.extend_from_slice(&word);
    // Head slot 2: access (uint8)
    let mut word = [0u8; 32];
    word[31] = access;
    out.extend_from_slice(&word);
    // Head slot 3: minStake (uint256)
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&min_stake_wei.to_be_bytes());
    out.extend_from_slice(&word);

    // Tail: name length + padded data
    let mut len_word = [0u8; 32];
    len_word[24..].copy_from_slice(&(name_bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(&len_word);
    out.extend_from_slice(name_bytes);
    out.extend(std::iter::repeat_n(0, name_padded_len - name_bytes.len()));

    // Tail: description length + padded data
    let mut len_word = [0u8; 32];
    len_word[24..].copy_from_slice(&(desc_bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(&len_word);
    out.extend_from_slice(desc_bytes);
    out.extend(std::iter::repeat_n(0, desc_padded_len - desc_bytes.len()));

    out
}

/// Encode `joinPool(uint256 poolId)` payable. Sender stakes msg.value.
pub fn encode_join_pool(pool_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("joinPool(uint256)"));
    let mut id_word = [0u8; 32];
    id_word[24..].copy_from_slice(&pool_id.to_be_bytes());
    out.extend_from_slice(&id_word);
    out
}

/// Encode `leavePool(uint256 poolId)`. Returns the user's stake.
pub fn encode_leave_pool(pool_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("leavePool(uint256)"));
    let mut id_word = [0u8; 32];
    id_word[24..].copy_from_slice(&pool_id.to_be_bytes());
    out.extend_from_slice(&id_word);
    out
}

/// Decode a bool return — Solidity bools are 32-byte words where
/// only the last byte is meaningful.
pub fn decode_bool(hex_result: &str) -> Option<bool> {
    let s = hex_result.strip_prefix("0x").unwrap_or(hex_result);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(bytes[31] != 0)
}

/// Decode a 32-byte big-endian uint256 (clamped to u128 since all
/// values we read — stake, pendingReward, claimable — fit comfortably).
pub fn decode_uint256_u128(hex_result: &str) -> Option<u128> {
    let s = hex_result.strip_prefix("0x").unwrap_or(hex_result);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    // If the high 16 bytes are non-zero the value exceeds u128 — we
    // clamp to u128::MAX so the UI still renders something sane.
    if bytes[..16].iter().any(|&b| b != 0) {
        return Some(u128::MAX);
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[16..]);
    Some(u128::from_be_bytes(buf))
}

/// Decode a 7-field ProviderProfile tuple (7 × 32 bytes = 224 bytes).
pub fn decode_provider_profile(hex_result: &str) -> Option<ProviderProfile> {
    let s = hex_result.strip_prefix("0x").unwrap_or(hex_result);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() < 32 * 7 {
        return None;
    }
    // Helper to read a word as u128 (small-integer fields fit easily).
    let read_u128 = |i: usize| -> u128 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[i * 32 + 16..i * 32 + 32]);
        u128::from_be_bytes(buf)
    };
    let read_u64 = |i: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[i * 32 + 24..i * 32 + 32]);
        u64::from_be_bytes(buf)
    };
    Some(ProviderProfile {
        is_registered: bytes[31] != 0, // word 0, last byte
        stake_wei: read_u128(1),
        total_jobs_completed: read_u64(2),
        total_jobs_failed: read_u64(3),
        reputation_bps: read_u64(4),
        current_active_jobs: read_u64(5),
        max_concurrent_jobs: read_u64(6),
    })
}

/// Tiny wei → SALT formatter (3-decimal display).
pub fn wei_to_salt_display(wei: u128) -> String {
    if wei == 0 {
        return "0".to_string();
    }
    let salt_int = wei / 1_000_000_000_000_000_000;
    let frac = (wei % 1_000_000_000_000_000_000) / 1_000_000_000_000_000;
    if frac == 0 {
        format!("{}", salt_int)
    } else {
        format!("{}.{:03}", salt_int, frac)
    }
}

/// Issue an eth_call and return the hex result string (with 0x
/// prefix). Returns Ok("0x") if the node replies with empty data,
/// Err on transport or RPC-level errors.
pub async fn eth_call(rpc_url: &str, to: &str, data: &[u8]) -> Result<String, String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [
            {
                "to": to,
                "data": format!("0x{}", hex::encode(data)),
            },
            "latest"
        ],
        "id": 1,
    });
    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("rpc transport: {}", e))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("rpc decode: {}", e))?;
    if let Some(err) = json.get("error") {
        return Err(format!("rpc error: {}", err));
    }
    json.get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "rpc missing result".to_string())
}

// ── CM-01 WP-01.5: Recent activity (eth_getLogs) ────────────────────
//
// Queries the three ComputeMarketplace events a provider cares about:
// JobAssigned, JobCompleted, JobFailed. All three share the shape
// `(uint256 indexed jobId, address indexed provider, ...)` so we can
// filter on topic[2] == provider_address_padded across all three
// event signatures in a single eth_getLogs call.
//
// Data source: eth_getLogs (standard JSON-RPC)
//   - address: ComputeMarketplace (from compute_marketplace_address)
//   - topics[0]: ANY of the three event sigs (OR filter)
//   - topics[2]: caller's provider address (left-pad to 32 bytes)
//   - fromBlock: latest - ACTIVITY_LOOKBACK_BLOCKS (bounded query)

/// How far back to look when fetching recent activity. 10,000 blocks at
/// 2 s/block ≈ 5.5 hours — enough for a "recent" view without being
/// expensive on archive nodes.
pub const ACTIVITY_LOOKBACK_BLOCKS: u64 = 10_000;

/// Maximum number of activity entries returned to the UI.
pub const ACTIVITY_LIMIT: usize = 10;

/// Compute a keccak256 topic hash for an event signature. Solidity
/// convention: strip parameter names, keep types; wrap in parens.
fn event_topic(sig: &str) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(sig.as_bytes());
    let out = h.finalize();
    let mut topic = [0u8; 32];
    topic.copy_from_slice(&out);
    topic
}

fn topic_job_assigned() -> [u8; 32] {
    event_topic("JobAssigned(uint256,address,uint256)")
}
fn topic_job_completed() -> [u8; 32] {
    event_topic("JobCompleted(uint256,address,uint256,uint256,uint256)")
}
fn topic_job_failed() -> [u8; 32] {
    event_topic("JobFailed(uint256,address)")
}

/// One row of a provider's recent activity, shaped for UI display.
/// Order: newest first (descending block number).
#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub job_id: u64,
    pub block_number: u64,
    pub status: String, // "Assigned", "Completed", or "Failed"
}

/// Fetch recent activity from ComputeMarketplace. Returns an empty
/// vec on any error (callers display an empty-state in the UI
/// regardless of the failure mode). Returns up to ACTIVITY_LIMIT
/// entries sorted newest-first.
pub async fn fetch_recent_activity(
    rpc_url: &str,
    market_address: &str,
    provider_address: &str,
) -> Result<Vec<ActivityEntry>, String> {
    // Address padded to 32 bytes per EVM log-filter convention.
    let padded = encode_address_padded(provider_address)
        .ok_or_else(|| "invalid provider address".to_string())?;
    let padded_hex = format!("0x{}", hex::encode(padded));

    // Three topic[0] candidates — eth_getLogs `topics[0]` as an array
    // ORs the values.
    let t_assigned = format!("0x{}", hex::encode(topic_job_assigned()));
    let t_completed = format!("0x{}", hex::encode(topic_job_completed()));
    let t_failed = format!("0x{}", hex::encode(topic_job_failed()));

    // First, resolve `latest` block number to bound the fromBlock
    // window. Without this, a fresh node with <10k blocks would
    // return an RPC error on `latest-10000`.
    let client = reqwest::Client::new();
    let bn_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_blockNumber",
        "params": [],
        "id": 1,
    });
    let bn_resp = client
        .post(rpc_url)
        .json(&bn_body)
        .send()
        .await
        .map_err(|e| format!("rpc transport: {}", e))?;
    let bn_json: serde_json::Value = bn_resp
        .json()
        .await
        .map_err(|e| format!("rpc decode: {}", e))?;
    let latest_hex = bn_json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing block number".to_string())?;
    let latest = u64::from_str_radix(latest_hex.trim_start_matches("0x"), 16)
        .map_err(|e| format!("bad block number: {}", e))?;
    let from_block = latest.saturating_sub(ACTIVITY_LOOKBACK_BLOCKS);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getLogs",
        "params": [{
            "fromBlock": format!("0x{:x}", from_block),
            "toBlock": "latest",
            "address": market_address,
            "topics": [
                [t_assigned, t_completed, t_failed],
                serde_json::Value::Null,
                padded_hex,
            ],
        }],
        "id": 2,
    });
    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("rpc transport: {}", e))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("rpc decode: {}", e))?;
    if let Some(err) = json.get("error") {
        return Err(format!("rpc error: {}", err));
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing logs array".to_string())?;

    let t_a = topic_job_assigned();
    let t_c = topic_job_completed();
    let t_f = topic_job_failed();

    let mut entries: Vec<ActivityEntry> = logs
        .iter()
        .filter_map(|log| {
            let topics = log.get("topics")?.as_array()?;
            if topics.len() < 3 {
                return None;
            }
            let topic0_hex = topics[0].as_str()?.trim_start_matches("0x");
            let topic0 = hex::decode(topic0_hex).ok()?;
            if topic0.len() != 32 {
                return None;
            }
            let topic0_arr: [u8; 32] = topic0.try_into().ok()?;
            let status = if topic0_arr == t_a {
                "Assigned"
            } else if topic0_arr == t_c {
                "Completed"
            } else if topic0_arr == t_f {
                "Failed"
            } else {
                return None;
            };

            // topic[1] is the indexed jobId (uint256 → u64 via last 8 bytes).
            let job_id_hex = topics[1].as_str()?.trim_start_matches("0x");
            let job_id_bytes = hex::decode(job_id_hex).ok()?;
            if job_id_bytes.len() != 32 {
                return None;
            }
            let mut job_id_buf = [0u8; 8];
            job_id_buf.copy_from_slice(&job_id_bytes[24..32]);
            let job_id = u64::from_be_bytes(job_id_buf);

            let block_hex = log.get("blockNumber")?.as_str()?.trim_start_matches("0x");
            let block_number = u64::from_str_radix(block_hex, 16).ok()?;

            Some(ActivityEntry {
                job_id,
                block_number,
                status: status.to_string(),
            })
        })
        .collect();

    // Descending by block number — newest first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.block_number));
    entries.truncate(ACTIVITY_LIMIT);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden selector tests. These lock in the 4-byte function
    // identifier for each ABI signature. If a selector changes, the
    // Solidity signature or our string literal is wrong — both break
    // on-chain calls silently. Values computed once from sha3/Keccak256
    // and frozen here.

    #[test]
    fn selector_register_provider() {
        let got = selector("registerProvider(bytes32[])");
        assert_eq!(hex::encode(got), "0589cd44");
    }

    #[test]
    fn get_provider_selector() {
        let got = selector("getProvider(address)");
        assert_eq!(hex::encode(got), "55f21eb7");
    }

    #[test]
    fn claimable_selector() {
        let got = selector("claimable(address)");
        assert_eq!(hex::encode(got), "402914f5");
    }

    #[test]
    fn claim_rewards_selector() {
        let got = selector("claimRewards()");
        assert_eq!(hex::encode(got), "372500ab");
    }

    // CM-01 WP-01.5 — event topic guards.
    //
    // Topic[0] is keccak256 of the Solidity event signature with
    // parameter names stripped. If any of these three strings drifts
    // from ComputeMarketplace.sol, eth_getLogs silently returns zero
    // rows and the Your-listing card looks permanently idle. These
    // tests lock in the exact sig string we filter on.

    #[test]
    fn topics_are_distinct_and_stable() {
        let a = topic_job_assigned();
        let c = topic_job_completed();
        let f = topic_job_failed();
        assert_ne!(a, c);
        assert_ne!(a, f);
        assert_ne!(c, f);
        // Determinism: same input → same output.
        assert_eq!(a, topic_job_assigned());
        assert_eq!(c, topic_job_completed());
        assert_eq!(f, topic_job_failed());
    }

    #[test]
    fn topic_job_assigned_signature() {
        // ComputeMarketplace.sol:192 declares
        // `event JobAssigned(uint256 indexed jobId, address indexed provider, uint256 price)`.
        // Topic[0] = keccak256("JobAssigned(uint256,address,uint256)").
        let t = topic_job_assigned();
        assert_eq!(t.len(), 32);
        // Sanity: fresh compute against the SAME sig string must match.
        assert_eq!(t, event_topic("JobAssigned(uint256,address,uint256)"));
        // And must NOT match a plausible-but-wrong signature (e.g.,
        // forgetting the trailing uint256).
        assert_ne!(t, event_topic("JobAssigned(uint256,address)"));
    }

    #[test]
    fn topic_job_completed_signature() {
        // ComputeMarketplace.sol:206 declares 5 fields.
        let t = topic_job_completed();
        assert_eq!(t.len(), 32);
        assert_eq!(
            t,
            event_topic("JobCompleted(uint256,address,uint256,uint256,uint256)")
        );
        // Guard against dropping a uint256 by accident.
        assert_ne!(
            t,
            event_topic("JobCompleted(uint256,address,uint256,uint256)")
        );
    }

    #[test]
    fn topic_job_failed_signature() {
        // ComputeMarketplace.sol:216 declares exactly 2 fields.
        let t = topic_job_failed();
        assert_eq!(t.len(), 32);
        assert_eq!(t, event_topic("JobFailed(uint256,address)"));
    }

    #[test]
    fn activity_entry_construction() {
        // Round-trip a fabricated entry to catch field-order changes.
        let e = ActivityEntry {
            job_id: 42,
            block_number: 1_234_567,
            status: "Completed".to_string(),
        };
        assert_eq!(e.job_id, 42);
        assert_eq!(e.block_number, 1_234_567);
        assert_eq!(e.status, "Completed");
    }

    #[test]
    fn encode_register_provider_layout() {
        let m = any_model_hash();
        let data = encode_register_provider(&[m]);
        // 4 selector + 32 offset + 32 length + 32 one element = 100
        assert_eq!(data.len(), 100);
        // offset = 0x20
        assert_eq!(data[4 + 31], 0x20);
        // length = 1
        assert_eq!(data[4 + 32 + 31], 1);
        // element == keccak256("any")
        assert_eq!(&data[4 + 64..4 + 96], &m[..]);
    }

    #[test]
    fn address_padding() {
        let addr = "0x1234567890abcdef1234567890abcdef12345678";
        let padded = encode_address_padded(addr).expect("valid addr");
        // First 12 bytes zero
        assert!(padded[..12].iter().all(|&b| b == 0));
        // Last 20 bytes = address
        assert_eq!(
            hex::encode(&padded[12..]),
            "1234567890abcdef1234567890abcdef12345678"
        );
    }

    #[test]
    fn address_padding_rejects_invalid() {
        assert!(encode_address_padded("0xnothex").is_none());
        assert!(encode_address_padded("0x12").is_none());
    }

    #[test]
    fn decode_uint256_roundtrip() {
        // 1000 SALT in wei = 0x3635c9adc5dea00000
        let hex = "0x00000000000000000000000000000000000000000000003635c9adc5dea00000";
        assert_eq!(decode_uint256_u128(hex), Some(MIN_PROVIDER_STAKE_WEI));
    }

    #[test]
    fn decode_uint256_zero() {
        let hex = "0x0000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_uint256_u128(hex), Some(0));
    }

    #[test]
    fn decode_uint256_overflow_clamps() {
        // All bits set → clamp to u128::MAX
        let hex = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert_eq!(decode_uint256_u128(hex), Some(u128::MAX));
    }

    #[test]
    fn decode_provider_profile_unregistered() {
        // All zeros → unregistered, zero everything
        let zeros = "0x".to_owned() + &"00".repeat(32 * 7);
        let p = decode_provider_profile(&zeros).expect("decode ok");
        assert!(!p.is_registered);
        assert_eq!(p.stake_wei, 0);
        assert_eq!(p.current_active_jobs, 0);
    }

    #[test]
    fn decode_provider_profile_registered() {
        // Craft: is_registered=1, stake=1000e18, jobs_completed=5,
        // jobs_failed=1, reputation=9500, active=2, max=10
        let mut hex_s = String::from("0x");
        hex_s.push_str(&format!("{:064x}", 1u64)); // is_registered
        hex_s.push_str(&format!("{:064x}", MIN_PROVIDER_STAKE_WEI)); // stake
        hex_s.push_str(&format!("{:064x}", 5u64));
        hex_s.push_str(&format!("{:064x}", 1u64));
        hex_s.push_str(&format!("{:064x}", 9500u64));
        hex_s.push_str(&format!("{:064x}", 2u64));
        hex_s.push_str(&format!("{:064x}", 10u64));
        let p = decode_provider_profile(&hex_s).expect("decode ok");
        assert!(p.is_registered);
        assert_eq!(p.stake_wei, MIN_PROVIDER_STAKE_WEI);
        assert_eq!(p.total_jobs_completed, 5);
        assert_eq!(p.total_jobs_failed, 1);
        assert_eq!(p.reputation_bps, 9500);
        assert_eq!(p.current_active_jobs, 2);
        assert_eq!(p.max_concurrent_jobs, 10);
    }

    #[test]
    fn wei_to_salt_fmt() {
        assert_eq!(wei_to_salt_display(0), "0");
        assert_eq!(wei_to_salt_display(1_000_000_000_000_000_000), "1");
        assert_eq!(wei_to_salt_display(1_500_000_000_000_000_000), "1.500");
        assert_eq!(wei_to_salt_display(MIN_PROVIDER_STAKE_WEI), "1000");
    }

    #[test]
    fn address_lookup() {
        assert!(compute_marketplace_address(40204).is_some());
        assert!(contribution_accounting_address(40204).is_some());
        assert!(learning_pool_address(40204).is_some());
        assert!(compute_marketplace_address(1).is_none());
        assert!(contribution_accounting_address(9999).is_none());
        assert!(learning_pool_address(1).is_none());
    }

    #[test]
    fn next_pool_id_selector() {
        assert_eq!(hex::encode(selector("nextPoolId()")), "18e56131");
    }

    #[test]
    fn encode_join_pool_layout() {
        let data = encode_join_pool(7);
        // 4-byte selector + 32-byte uint256
        assert_eq!(data.len(), 36);
        // pool id 7 in the last byte of the second word
        assert_eq!(data[35], 7);
        assert!(
            data[4..35].iter().all(|&b| b == 0),
            "id should be left-padded with zeros"
        );
    }

    #[test]
    fn encode_is_member_layout() {
        let addr = "0x1234567890abcdef1234567890abcdef12345678";
        let data = encode_is_member(3, addr).expect("valid addr");
        // 4 + 32 (poolId) + 32 (address) = 68
        assert_eq!(data.len(), 68);
        assert_eq!(data[35], 3);
        assert_eq!(
            hex::encode(&data[48..68]),
            "1234567890abcdef1234567890abcdef12345678"
        );
    }

    #[test]
    fn encode_stakes_layout() {
        let addr = "0xabababababababababababababababababababab";
        let data = encode_stakes(0, addr).expect("valid addr");
        assert_eq!(data.len(), 68);
        // pool id 0 → all zero in the second word
        assert!(data[4..36].iter().all(|&b| b == 0));
    }

    #[test]
    fn decode_bool_true_false() {
        let true_hex = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let false_hex = "0x0000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_bool(true_hex), Some(true));
        assert_eq!(decode_bool(false_hex), Some(false));
    }

    #[test]
    fn decode_bool_rejects_short() {
        assert!(decode_bool("0x01").is_none());
    }

    /// Independent re-parse of the vendored canonical table
    /// (`src/generated/addresses.json`). Tests compare the public
    /// helpers against THIS, never against hardcoded address literals —
    /// literals drift across re-rolls (NATIVE-R1-S2 WP-A4: the previous
    /// `0xf62ab4…5283` ComputeMarketplace pin had NO code on chain
    /// 40204, while the book's `0xd7a20b…d599` did — verified via
    /// eth_getCode on rpc.citrate.ai, 2026-07-04). On-chain validity of
    /// the book itself is covered by `tests/live_addresses.rs`.
    fn book_address(name: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(ADDRESS_TABLE_JSON)
            .expect("vendored src/generated/addresses.json parses");
        for section in ["contracts", "aaStack"] {
            if let Some(addr) = v[section].get(name).and_then(|a| a.as_str()) {
                return addr.to_string();
            }
        }
        panic!(
            "{name:?} not found in src/generated/addresses.json \
             (stale table? run `bash scripts/sync-addresses.sh`)"
        );
    }

    #[test]
    fn known_contract_returns_listed_addresses() {
        // Spot-check well-known names resolve to the book's entries.
        // WP-A4 2026-07-04: no literals — the vendored
        // generated/addresses.json is the single source; citrate-chain's
        // DEPLOYED_ADDRESSES.md still carries the stale pre-reroll set
        // and needs its own reconcile (flagged to the chain repo).
        for name in ["ModelRegistry", "ComputeMarketplace", "LearningPool"] {
            assert_eq!(
                known_contract(40204, name),
                Some(book_address(name).as_str()),
                "known_contract(40204, {name:?}) diverged from the vendored book"
            );
        }
        // Unknown name returns None
        assert!(known_contract(40204, "NotARealContract").is_none());
        // Wrong chain returns None
        assert!(known_contract(1, "ModelRegistry").is_none());
    }

    #[test]
    fn known_contract_names_match_canonical() {
        // Every name surfaced by `known_contract_names` resolves to an
        // address via `known_contract` — the two views of the canonical
        // table must agree. The exact count comes from the vendored
        // canonical (contracts + aaStack), so this test asserts only
        // the round-trip invariant; the canonical's own tests pin the
        // address values.
        let names = known_contract_names(40204);
        assert!(
            !names.is_empty(),
            "vendored src/generated/addresses.json is empty — run `bash scripts/sync-addresses.sh`"
        );
        for name in names {
            assert!(
                known_contract(40204, name).is_some(),
                "name {name:?} in known_contract_names but not in known_contract"
            );
        }
        // Compute-critical names the gui drives daily must be present
        // regardless of how the canonical evolves — pin them explicitly.
        for required in [
            "ModelRegistry",
            "InferenceRouter",
            "ComputeMarketplace",
            "ComputePool",
            "ContributionAccounting",
            "LearningPool",
        ] {
            assert!(
                names.contains(&required),
                "compute-critical name {required:?} missing from canonical (vendored addresses.json may be stale)"
            );
        }
    }

    #[test]
    fn model_count_selector() {
        assert_eq!(hex::encode(selector("getModelCount()")), "4989dbb0");
    }

    #[test]
    fn encode_get_model_at_layout() {
        let data = encode_get_model_at(7);
        assert_eq!(data.len(), 36);
        assert_eq!(data[35], 7);
        assert!(data[4..35].iter().all(|&b| b == 0));
    }

    #[test]
    fn create_pool_selector() {
        // keccak256("createPool(string,string,uint8,uint256)") prefix
        let got = selector("createPool(string,string,uint8,uint256)");
        assert_eq!(got.len(), 4);
        // Computed once and frozen
        assert_eq!(hex::encode(got), "c9b37ca5");
    }

    #[test]
    fn encode_create_pool_short_strings() {
        let data = encode_create_pool("A", "B", 0, MIN_PROVIDER_STAKE_WEI);
        // Selector + 4 head words + name (32 len + 32 padded data) + desc (32 + 32) = 4 + 128 + 64 + 64 = 260
        assert_eq!(data.len(), 260);
        // Name offset = 0x80 (128 dec)
        assert_eq!(data[4 + 31], 0x80);
        // Desc offset = 128 + 32 + 32 = 192 (0xc0)
        assert_eq!(data[4 + 32 + 31], 0xc0);
        // Access slot is all zeros (Open)
        assert!(data[4 + 64..4 + 96].iter().all(|&b| b == 0));
        // Name length = 1
        assert_eq!(data[4 + 128 + 31], 1);
        // Name char = 'A'
        assert_eq!(data[4 + 128 + 32], b'A');
    }

    #[test]
    fn encode_create_pool_aligns_to_32() {
        // 33-byte name should pad to 64 bytes of data
        let long_name = "a".repeat(33);
        let data = encode_create_pool(&long_name, "x", 0, 1000);
        // Selector + 128 head + 32 (len) + 64 (padded) + 32 (len) + 32 (padded) = 4 + 128 + 96 + 64 = 292
        assert_eq!(data.len(), 292);
    }

    #[test]
    fn encode_create_pool_access_byte() {
        let data = encode_create_pool("p", "q", 2, 1000); // ApplicationRequired
        assert_eq!(data[4 + 64 + 31], 2);
    }

    #[test]
    fn get_pool_selector() {
        let got = selector("getPool(uint256)");
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn encode_get_pool_layout() {
        let data = encode_get_pool(5);
        assert_eq!(data.len(), 36);
        assert_eq!(data[35], 5);
    }

    #[test]
    fn decode_pool_info_roundtrip() {
        // Hand-build a Pool encoding:
        //   outer offset = 0x20
        //   head (9 × 32):
        //     id=7
        //     name_offset (relative to tuple start)
        //     desc_offset
        //     creator=0x1234...5678
        //     state=0 access=0 minStake=1000e18 memberCount=3 createdAt=1700000000
        //   tail:
        //     name "MyPool" (6 bytes)  → len + 32 padded data
        //     desc "A cool pool" (11 bytes) → len + 32 padded data
        //
        // Head size = 9 * 32 = 288 bytes.
        // name_offset = 288 (first byte of tail). After name: +32 (len) + 32 (padded) = 64
        // desc_offset = 288 + 64 = 352
        let mut hex_s = String::from("0x");
        hex_s.push_str(&format!("{:064x}", 0x20)); // outer offset
                                                   // Head
        hex_s.push_str(&format!("{:064x}", 7u64)); // id
        hex_s.push_str(&format!("{:064x}", 288u64)); // name_offset
        hex_s.push_str(&format!("{:064x}", 352u64)); // desc_offset
                                                     // creator
        hex_s.push_str("000000000000000000000000");
        hex_s.push_str("1234567890123456789012345678901234567890");
        hex_s.push_str(&format!("{:064x}", 0u64)); // state
        hex_s.push_str(&format!("{:064x}", 0u64)); // access
        hex_s.push_str(&format!("{:064x}", MIN_PROVIDER_STAKE_WEI));
        hex_s.push_str(&format!("{:064x}", 3u64)); // memberCount
        hex_s.push_str(&format!("{:064x}", 1700000000u64));
        // Tail: name
        hex_s.push_str(&format!("{:064x}", 6u64)); // name length
        hex_s.push_str("4d79506f6f6c"); // "MyPool" hex
        hex_s.push_str(&"0".repeat(52)); // pad to 32
                                         // Tail: description
        hex_s.push_str(&format!("{:064x}", 11u64)); // desc length
        hex_s.push_str("4120636f6f6c20706f6f6c"); // "A cool pool" hex
        hex_s.push_str(&"0".repeat(42)); // pad to 32

        let pool = decode_pool_info(&hex_s).expect("decode ok");
        assert_eq!(pool.id, 7);
        assert_eq!(pool.name, "MyPool");
        assert_eq!(pool.description, "A cool pool");
        assert_eq!(pool.creator, "0x1234567890123456789012345678901234567890");
        assert_eq!(pool.min_stake_wei, MIN_PROVIDER_STAKE_WEI);
        assert_eq!(pool.member_count, 3);
        assert_eq!(pool.created_at, 1700000000);
    }

    #[test]
    fn decode_pool_info_rejects_short() {
        assert!(decode_pool_info("0x00").is_none());
    }

    #[test]
    fn decode_bytes32_roundtrip() {
        let hex = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        assert_eq!(decode_bytes32(hex), Some(hex.to_string()));
        // Wrong length → None
        assert!(decode_bytes32("0x12").is_none());
    }

    // ── Canonical address-book tripwire ──────────────────────────────
    //
    // A prior reroll left a hand-maintained address map systematically
    // mis-mapped — names pointed at the *next* contract's address —
    // which silently routed on-chain calls (incl. provider registration
    // via "List on marketplace") to the wrong/stale contracts. And a
    // later drift (WP-A4) left test literals pinning addresses that had
    // NO code on-chain. The rule now: `src/generated/addresses.json` is
    // the ONLY place a contract address may be written. These tests
    // assert (a) every public helper faithfully exposes the book and
    // (b) no 0x-40-hex literal anywhere in `src/**/*.rs` contradicts
    // it. On-chain validity of the book is enforced by
    // `tests/live_addresses.rs` (eth_getCode over every entry).

    #[test]
    fn canonical_address_book_compute_critical() {
        // Compute-critical names the GUI drives daily: each must be
        // present in the vendored book and exposed verbatim by
        // `known_contract`. No literals — see WP-A4 note on
        // `book_address` (the old 0xf62ab4…5283 / 0xd1b723…6c99 /
        // 0x61bc73…253e pins were empty on-chain).
        for name in [
            "ComputeMarketplace",
            "ComputePool",
            "ComputeVerifier",
            "HeartbeatMonitor",
            "ComputePricingOracle",
            "ContributionAccounting",
            "BulkComputeGateway",
            "ModelRegistry",
            "WrappedSALT",
            "InferenceRouter",
        ] {
            assert_eq!(
                known_contract(40204, name),
                Some(book_address(name).as_str()),
                "known_contract(40204, {name:?}) diverged from \
                 src/generated/addresses.json"
            );
        }
    }

    #[test]
    fn compute_marketplace_helper_is_canonical() {
        // The dedicated helper drives the "List on marketplace" /
        // registerProvider path. It MUST equal the canonical book's
        // ComputeMarketplace entry — and the per-name map entry.
        assert_eq!(
            compute_marketplace_address(40204),
            Some(book_address("ComputeMarketplace").as_str())
        );
        assert_eq!(
            compute_marketplace_address(40204),
            known_contract(40204, "ComputeMarketplace")
        );
    }

    #[test]
    fn no_source_address_literal_contradicts_the_book() {
        // WP-A4 tripwire: scan every `src/**/*.rs` file for 0x-40-hex
        // literals. Each one must either be a value present in the
        // vendored canonical book (any section — contracts, aaStack,
        // precompiles, genesis, deployer) or sit on the explicit
        // fixture allowlist below. A contract pin that drifts from the
        // book is neither, and fails here instead of shipping a
        // wrong-contract footgun.
        const ALLOWED_FIXTURES: &[&str] = &[
            // ABI-encoding fixtures (marketplace_client.rs tests)
            "0x1234567890abcdef1234567890abcdef12345678",
            "0xabababababababababababababababababababab",
            "0x1234567890123456789012345678901234567890",
            // Known-stale sentinel asserted ABSENT in
            // `stale_compute_marketplace_address_is_gone`
            "0x8951ae72e5479cae28ef7bb3caa4207d5719e24b",
            // Visual-test wallet fixture (ui_visual_tests.rs)
            "0xaceaa7d00c024d32e6e0a07094ceb1a7706786d1",
            // EIP-55 spec example address — recipient-validation fixture
            // in main.rs `natb012_recipient_validation`.
            "0x52908400098527886e0f7030069857d2e4169ee7",
            // Model precompile (app_binder.rs registerModel target). Not a
            // deployed contract: hardcoded in citrate-execution
            // `Executor::model_precompile_address()`, so it cannot drift on
            // redeploy. It was in the book as `precompiles.StateModel` until
            // the 2026-09-23 resync to citrate-chain 40204.json, which no
            // longer emits the executor-native 0x10xx precompiles.
            "0x0000000000000000000000000000000000001000",
        ];

        // Every 40-hex string value anywhere in the book, lowercased.
        fn collect_book_values(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::String(s) => {
                    if is_addr_literal(s) {
                        out.push(s.to_ascii_lowercase());
                    }
                }
                serde_json::Value::Object(m) => {
                    m.values().for_each(|v| collect_book_values(v, out))
                }
                serde_json::Value::Array(a) => a.iter().for_each(|v| collect_book_values(v, out)),
                _ => {}
            }
        }
        fn is_addr_literal(s: &str) -> bool {
            s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
        }
        // Extract exactly-40-hex 0x literals (longer hex runs — tx
        // hashes, topics, bytes32 — are skipped).
        fn extract_addr_literals(text: &str) -> Vec<String> {
            let bytes = text.as_bytes();
            let mut found = Vec::new();
            let mut i = 0;
            while i + 1 < bytes.len() {
                if bytes[i] == b'0' && bytes[i + 1] == b'x' {
                    let start = i + 2;
                    let mut end = start;
                    while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                        end += 1;
                    }
                    if end - start == 40 {
                        found.push(text[i..end].to_ascii_lowercase());
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            found
        }
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("src dir readable") {
                let path = entry.expect("dir entry readable").path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let book: serde_json::Value = serde_json::from_str(ADDRESS_TABLE_JSON)
            .expect("vendored src/generated/addresses.json parses");
        let mut book_values = Vec::new();
        collect_book_values(&book, &mut book_values);
        assert!(!book_values.is_empty(), "book has no addresses at all?");

        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src_root, &mut files);
        assert!(!files.is_empty(), "no .rs files under {src_root:?}?");

        let mut offenders = Vec::new();
        for path in &files {
            let text =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            for lit in extract_addr_literals(&text) {
                if !book_values.contains(&lit) && !ALLOWED_FIXTURES.contains(&lit.as_str()) {
                    offenders.push(format!("{} in {}", lit, path.display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "address literal(s) in src/ that are neither in \
             src/generated/addresses.json nor allowlisted fixtures — \
             contract addresses must come from the book (run `bash \
             scripts/sync-addresses.sh` if the book is stale, or extend \
             ALLOWED_FIXTURES for a genuine test fixture): {offenders:#?}"
        );
    }

    #[test]
    fn stale_compute_marketplace_address_is_gone() {
        // 0x8951…e24b is NOT in the canonical table — it must not appear
        // anywhere the GUI resolves contract addresses.
        const STALE: &str = "0x8951ae72e5479cae28ef7bb3caa4207d5719e24b";
        assert_ne!(compute_marketplace_address(40204), Some(STALE));
        for name in known_contract_names(40204) {
            assert_ne!(
                known_contract(40204, name),
                Some(STALE),
                "stale address resurfaced under name {:?}",
                name
            );
        }
    }
}
