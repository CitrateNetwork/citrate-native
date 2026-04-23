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

/// Known marketplace deployments by chain ID. Source of truth:
/// `contracts/DEPLOYED_ADDRESSES.md` (post-reroll 2026-04-22).
///
/// Add new entries here as rerolls happen. Unknown chain IDs return
/// `None` — the UI surfaces that as "Marketplace not deployed on
/// this network" rather than silently pointing at a zero address.
pub fn compute_marketplace_address(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        // testnet-beta (2026-04-22 reroll)
        40204 => Some("0x8951ae72e5479cae28ef7bb3caa4207d5719e24b"),
        _ => None,
    }
}

pub fn contribution_accounting_address(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        40204 => Some("0x1afe987622ab5add275d2fd21248f77f5e00667f"),
        _ => None,
    }
}

pub fn learning_pool_address(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        40204 => Some("0x9a58e44f8dd6fd6a75637a32e6e51c16440996f8"),
        _ => None,
    }
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
pub async fn eth_call(
    rpc_url: &str,
    to: &str,
    data: &[u8],
) -> Result<String, String> {
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
        assert_eq!(hex::encode(&padded[12..]), "1234567890abcdef1234567890abcdef12345678");
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
        assert!(data[4..35].iter().all(|&b| b == 0), "id should be left-padded with zeros");
    }

    #[test]
    fn encode_is_member_layout() {
        let addr = "0x1234567890abcdef1234567890abcdef12345678";
        let data = encode_is_member(3, addr).expect("valid addr");
        // 4 + 32 (poolId) + 32 (address) = 68
        assert_eq!(data.len(), 68);
        assert_eq!(data[35], 3);
        assert_eq!(hex::encode(&data[48..68]), "1234567890abcdef1234567890abcdef12345678");
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
}
