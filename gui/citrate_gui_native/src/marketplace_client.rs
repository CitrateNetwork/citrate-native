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

pub fn model_registry_address(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        40204 => Some("0x077fbc3338a9e6bad90a3a041e6b7425689754ef"),
        _ => None,
    }
}

/// Full address book for testnet-beta (chain 40204). T2-7 — keeps
/// the GUI from being a stranger to the rest of the 36 deployed
/// contracts. Future panels and chat tools resolve names via this
/// map instead of hardcoding addresses one-by-one.
///
/// Source of truth: `contracts/DEPLOYED_ADDRESSES.md` (post-reroll
/// 2026-04-22). When the address list changes, regenerate this
/// table from the markdown.
#[allow(dead_code)]  // Used by future panels + chat tools (T2-7 forward infra)
pub fn known_contract(chain_id: u64, name: &str) -> Option<&'static str> {
    if chain_id != 40204 {
        return None;
    }
    match name {
        "ModelRegistry"            => Some("0x077fbc3338a9e6bad90a3a041e6b7425689754ef"),
        "WrappedSALT"              => Some("0x1f73bb479f397a34b5e3145e51d25bc5007273bf"),
        "AgentDecisionRegistry"    => Some("0x0aaa6e00fcab1da5599f6dce86e361a5e03a5759"),
        "SpecRegistry"             => Some("0x1b6aeed728f53b48e1ed831b04a1f4812f48e928"),
        "IPFSIncentives"           => Some("0xa6a4122126a75611ea06241e404327addfe8eb5e"),
        "X402Facilitator"          => Some("0xc0fde3a8a42f6479cf12b4a5489e7a988c918e23"),
        "X402Paywall"              => Some("0x11399989175783cdca8ecb095835c8cd4720c6fc"),
        "LiquidStakingPool"        => Some("0xd71b7e33e447e062f4e796def686156805820b29"),
        "ContributionAccounting"   => Some("0x1afe987622ab5add275d2fd21248f77f5e00667f"),
        "NematocystSlashing"       => Some("0x425064443c3c3392c47dcbe10d455831545efd9b"),
        "MarketMakerAllocation"    => Some("0xf61e79af3bc2a905695e45b0fa7a43f9141a554a"),
        "ModelMarketplace"         => Some("0x11a5e6f57751d8fa1c5b58ad2bf13528160985f0"),
        "InferenceRouter"          => Some("0xad7c3135c1b9b3189208fd617b6b058c1c0469f3"),
        "LoRAFactory"              => Some("0xac6bfb1709bcba5a005fe2823b4d8bc55db2b7d9"),
        "LearningPool"             => Some("0x9a58e44f8dd6fd6a75637a32e6e51c16440996f8"),
        "LearningCycleManager"     => Some("0x20a0b74c766e84b20558abd76a7a0fd6434a4c4c"),
        "ClassroomRegistry"        => Some("0x7e7a3db3be6fe4bea06acdbb772786432e1293e3"),
        "ComputeVerifier"          => Some("0xd29d4d059808adc43b761f41c675f1eb546e1a19"),
        "ComputeMarketplace"       => Some("0x8951ae72e5479cae28ef7bb3caa4207d5719e24b"),
        "ComputePool"              => Some("0x86d918808b48ad543c9c816b5303b7dbcb0e321f"),
        "HeartbeatMonitor"         => Some("0xf3f9f72ea2bb3f763b07390b7257da643b8ee9b6"),
        "DisputeResolution"        => Some("0x8b36c15552394ce44173a29d054dc5ca482e65d3"),
        "ComputePricingOracle"     => Some("0x46773aeca885be65cd313b7d9bce9625767d40b5"),
        "StablecoinTreasury"       => Some("0x6884ef1907468a13265a0bbb67da20ef4b52199b"),
        "BulkComputeGateway"       => Some("0xa1eed6ae021504e2a1e310e6c0f7c1a0c5bf4647"),
        "TestnetFarmingAccounting" => Some("0x828c6b831c4ce08170bc3efc6f6026dc44b20dfa"),
        "TreasuryGovernor"         => Some("0x7efc1eb17beff413e1af7fb3bb541e895c307300"),
        "AIModelRegistryPortable"  => Some("0x541923570df41b307ca037fdd0fb508502885455"),
        "AIInferenceRouterPortable"=> Some("0x516380b0acef9a9541641c85dbe0bf89b3e56977"),
        "AILearningCycleCorePortable" => Some("0x26333384a517c50d8b116979490b4ad1506f1f9a"),
        "InstitutionalVault"       => Some("0x18d3e03eb3364f63db8e4f6bbd078ad8098c2c2b"),
        "ClassroomClusterV1"       => Some("0x00132c0f7fad65a6d54d2c561dc4609237437449"),
        "Forwarder"                => Some("0xcb5fcad35f892e7e1da4bb4d17a48dd9e056583e"),
        "BudgetAllocation"         => Some("0xdaff2b9dc254b6cb3040f8f14304d30e136fa136"),
        "CashoutRequest"           => Some("0xb87a4f754ca316d2416553d04f4eded26424b536"),
        "ModelAccessControl"       => Some("0x4ee0bef59a87a9ea3f91b80fd68ebfe69e72075a"),
        _ => None,
    }
}

/// All deployed contract names known on this chain. Used by the
/// chat agent and any panel that wants to enumerate the address
/// book ("what contracts are out there?").
#[allow(dead_code)]  // Used by future panels + chat tools (T2-7 forward infra)
pub fn known_contract_names(chain_id: u64) -> &'static [&'static str] {
    if chain_id != 40204 {
        return &[];
    }
    &[
        "ModelRegistry", "WrappedSALT", "AgentDecisionRegistry", "SpecRegistry",
        "IPFSIncentives", "X402Facilitator", "X402Paywall", "LiquidStakingPool",
        "ContributionAccounting", "NematocystSlashing", "MarketMakerAllocation",
        "ModelMarketplace", "InferenceRouter", "LoRAFactory", "LearningPool",
        "LearningCycleManager", "ClassroomRegistry", "ComputeVerifier",
        "ComputeMarketplace", "ComputePool", "HeartbeatMonitor", "DisputeResolution",
        "ComputePricingOracle", "StablecoinTreasury", "BulkComputeGateway",
        "TestnetFarmingAccounting", "TreasuryGovernor", "AIModelRegistryPortable",
        "AIInferenceRouterPortable", "AILearningCycleCorePortable",
        "InstitutionalVault", "ClassroomClusterV1", "Forwarder", "BudgetAllocation",
        "CashoutRequest", "ModelAccessControl",
    ]
}

// ── ModelRegistry ABI helpers (T2-6) ─────────────────────────────

/// Encode `getModelCount() → uint256`. ModelRegistry exposes the
/// total number of registered models so the GUI can paginate.
pub fn encode_model_count() -> Vec<u8> {
    selector("getModelCount()").to_vec()
}

/// Encode `getModelAt(uint256 index) → bytes32 modelId`. Returns
/// the ID of the Nth model in registration order.
#[allow(dead_code)]  // Used by future ModelRegistry browser (T2-6 forward infra)
pub fn encode_get_model_at(index: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(&selector("getModelAt(uint256)"));
    let mut idx_word = [0u8; 32];
    idx_word[24..].copy_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&idx_word);
    out
}

/// Decode a 32-byte bytes32 result into a hex string with 0x prefix.
#[allow(dead_code)]  // Used by future ModelRegistry browser (T2-6 forward infra)
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
#[allow(dead_code)]  // consumed by the Learning panel hydration
pub struct PoolInfo {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub creator: String,     // 0x-prefixed EIP-55 checksum-less hex
    pub state: u8,           // 0=Active, 1=Closed, 2=ActiveCycle
    pub access: u8,          // 0=Open, 1=InviteOnly, 2=ApplicationRequired
    pub min_stake_wei: u128,
    pub member_count: u64,
    pub created_at: u64,     // unix seconds
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
        if bytes.len() < pos + 32 { return None; }
        let len = u64::from_be_bytes(bytes[pos + 24..pos + 32].try_into().ok()?) as usize;
        if bytes.len() < pos + 32 + len { return None; }
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

    let mut out = Vec::with_capacity(
        4 + head_size as usize + 32 + name_padded_len + 32 + desc_padded_len
    );
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

    #[test]
    fn known_contract_returns_listed_addresses() {
        // Spot-check a few well-known names
        assert_eq!(known_contract(40204, "ModelRegistry"),
            Some("0x077fbc3338a9e6bad90a3a041e6b7425689754ef"));
        assert_eq!(known_contract(40204, "ComputeMarketplace"),
            Some("0x8951ae72e5479cae28ef7bb3caa4207d5719e24b"));
        assert_eq!(known_contract(40204, "LearningPool"),
            Some("0x9a58e44f8dd6fd6a75637a32e6e51c16440996f8"));
        // Unknown name returns None
        assert!(known_contract(40204, "NotARealContract").is_none());
        // Wrong chain returns None
        assert!(known_contract(1, "ModelRegistry").is_none());
    }

    #[test]
    fn known_contract_names_count_matches_address_book() {
        // 36 contracts deployed; the names list should match.
        assert_eq!(known_contract_names(40204).len(), 36);
        // Every name should resolve to an address
        for name in known_contract_names(40204) {
            assert!(known_contract(40204, name).is_some(),
                "Name '{}' in known_contract_names but not in known_contract", name);
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
        hex_s.push_str(&format!("{:064x}", 7u64));    // id
        hex_s.push_str(&format!("{:064x}", 288u64));  // name_offset
        hex_s.push_str(&format!("{:064x}", 352u64));  // desc_offset
        // creator
        hex_s.push_str("000000000000000000000000");
        hex_s.push_str("1234567890123456789012345678901234567890");
        hex_s.push_str(&format!("{:064x}", 0u64));    // state
        hex_s.push_str(&format!("{:064x}", 0u64));    // access
        hex_s.push_str(&format!("{:064x}", MIN_PROVIDER_STAKE_WEI));
        hex_s.push_str(&format!("{:064x}", 3u64));    // memberCount
        hex_s.push_str(&format!("{:064x}", 1700000000u64));
        // Tail: name
        hex_s.push_str(&format!("{:064x}", 6u64));    // name length
        hex_s.push_str("4d79506f6f6c"); // "MyPool" hex
        hex_s.push_str(&"0".repeat(52)); // pad to 32
        // Tail: description
        hex_s.push_str(&format!("{:064x}", 11u64));   // desc length
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
}
