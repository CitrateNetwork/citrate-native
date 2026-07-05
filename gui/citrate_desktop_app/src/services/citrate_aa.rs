//! EW-S1 WP-8 — ERC-4337 / Kernel v3 helpers for the "Link this device"
//! flow and the smart-wallet (paymaster-sponsored) send mode.
//!
//! Mirrors — byte-for-byte — the three sibling implementations:
//! on-chain `CitrateWalletFactory.predictAddress`, citrate-identity
//! `src/aa/wallet-claims.ts` + `predict.ts`, and citrate-sdk-js
//! `src/aa/*`. Parity is pinned by the unit tests below against
//! LIVE chain-40204 `eth_call` captures and citrate-sdk-js-generated
//! calldata vectors.
//!
//! NOTE on reuse (Rule 12 / manifest): the canonical Rust home for the
//! prediction + permit helpers is the `citrate-wallet-aa` crate in
//! citrate-chain. This workspace pins citrate-chain at rev `0f2d16b…`,
//! which predates that crate, and rev bumps ride the chain-workspace
//! upgrade train — so the (small, vector-pinned) helpers live here
//! until the next pin bump, at which point this module collapses to a
//! re-export. Tracked in the EW-S1 sprint file (WP-8 notes).

use sha3::{Digest, Keccak256};

/// Canonical chain-40204 AA addresses.
/// Source: citrate-chain `contracts/addresses/40204.json` → `aaStack`
/// (CREATE2 deterministic, reroll-stable; commits df62052/6d4f308).
pub mod addresses {
    pub const CHAIN_ID: u64 = 40204;
    pub const ENTRY_POINT: &str = "0x077Fbc3338A9e6BAD90A3A041E6b7425689754Ef";
    pub const FACTORY: &str = "0x9C0C25D4355FAE68679711ea99b7642BF3E9a68A";
    pub const WALLET_IMPL: &str = "0xe641A41b02F1ff114481D357D148bE4519830087";
    pub const PAYMASTER: &str = "0x884c47518a21496D17d4Dae62e96239B06177D28";
    pub const ECDSA_VALIDATOR: &str = "0xd2d35421379ae5b461e216bfcdd1b7e6a64bbc40";
    pub const WEBAUTHN_VALIDATOR: &str = "0x97ff6d1c4d2f4337ec09f2a1c01808016f728def";
    pub const GUARDIAN_RECOVERY: &str = "0x381B5848f3B5d73FF67b745624780a43682456Ce";
}

/// Selectors verified with `cast sig` against the canonical signatures.
const SEL_INITIALIZE: &str = "3c3b752b"; // initialize(bytes21,address,bytes,bytes,bytes[])
const SEL_DEPLOY_FOR: &str = "89ebe13b"; // deployFor(bytes32,address,bytes,uint256,bytes)
const SEL_EXECUTE: &str = "e9ae5c53"; // execute(bytes32,bytes)
const SEL_GET_NONCE: &str = "35567e1a"; // getNonce(address,uint192)

/// `CitrateECDSAValidator` install source tag for this surface.
pub const ECDSA_SOURCE_GUI_NATIVE: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum AaError {
    #[error("not a canonical UUID: {0}")]
    BadUuid(String),
    #[error("bad hex input: {0}")]
    BadHex(String),
    #[error("{0}")]
    Invalid(String),
}

fn keccak(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

fn strip0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, AaError> {
    hex::decode(strip0x(s)).map_err(|e| AaError::BadHex(format!("{s}: {e}")))
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

/// `keccak256(utf8(lowercase uuid))` — the cross-surface userId mapping
/// (identical to citrate-identity `uuidToUserId` + citrate-sdk-js).
pub fn uuid_to_user_id(uuid: &str) -> Result<[u8; 32], AaError> {
    let canonical = uuid.trim().to_lowercase();
    if !is_uuid(&canonical) {
        return Err(AaError::BadUuid(uuid.to_string()));
    }
    Ok(keccak(canonical.as_bytes()))
}

/// OIDC accountId (32-byte hex | 20-byte EOA | UUID) → 32-byte AA userId.
pub fn account_id_to_user_id(account_id: &str) -> Result<[u8; 32], AaError> {
    let s = account_id.trim();
    if let Some(h) = s.strip_prefix("0x") {
        let bytes = hex::decode(h).map_err(|e| AaError::BadHex(format!("{s}: {e}")))?;
        return match bytes.len() {
            32 => {
                let mut out = [0u8; 32];
                out.copy_from_slice(&bytes);
                Ok(out)
            }
            20 => {
                let mut out = [0u8; 32];
                out[12..].copy_from_slice(&bytes);
                Ok(out)
            }
            n => Err(AaError::Invalid(format!("accountId hex must be 20 or 32 bytes (got {n})"))),
        };
    }
    uuid_to_user_id(s)
}

/// keccak256 of Solady's 95-byte minimal ERC-1967 clone init code with
/// the implementation embedded (constants pinned to upstream LibClone).
pub fn erc1967_init_code_hash(implementation: &str) -> Result<[u8; 32], AaError> {
    let impl_bytes = hex_to_bytes(implementation)?;
    if impl_bytes.len() != 20 {
        return Err(AaError::Invalid("implementation must be 20 bytes".into()));
    }
    let mut init = Vec::with_capacity(95);
    init.extend_from_slice(&hex_to_bytes("603d3d8160223d3973")?);
    init.extend_from_slice(&impl_bytes);
    init.extend_from_slice(&hex_to_bytes("6009")?);
    init.extend_from_slice(&hex_to_bytes(
        "5155f3363d3d373d3d363d7f360894a13ba1a3210667c828492db98dca3e2076",
    )?);
    init.extend_from_slice(&hex_to_bytes(
        "cc3735a920a3ca505d382bbc545af43d6000803e6038573d6000fd5b3d6000f3",
    )?);
    Ok(keccak(&init))
}

/// CREATE2 address the factory deploys the user's Kernel proxy to
/// (`salt = keccak256(userId)`). Returns a lowercase 0x address.
pub fn predict_wallet_address(
    factory: &str,
    implementation: &str,
    user_id: &[u8; 32],
) -> Result<String, AaError> {
    let factory_bytes = hex_to_bytes(factory)?;
    if factory_bytes.len() != 20 {
        return Err(AaError::Invalid("factory must be 20 bytes".into()));
    }
    let salt = keccak(user_id);
    let init_code_hash = erc1967_init_code_hash(implementation)?;
    let mut packed = Vec::with_capacity(1 + 20 + 32 + 32);
    packed.push(0xff);
    packed.extend_from_slice(&factory_bytes);
    packed.extend_from_slice(&salt);
    packed.extend_from_slice(&init_code_hash);
    let h = keccak(&packed);
    Ok(format!("0x{}", hex::encode(&h[12..])))
}

// ── ABI building blocks (head/tail words) ───────────────────────────

fn word_u(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_u64(v: u64) -> [u8; 32] {
    word_u(v as u128)
}

fn word_addr(addr: &str) -> Result<[u8; 32], AaError> {
    let b = hex_to_bytes(addr)?;
    if b.len() != 20 {
        return Err(AaError::Invalid(format!("address must be 20 bytes: {addr}")));
    }
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&b);
    Ok(w)
}

/// ABI dynamic-bytes tail: length word + right-padded content.
fn bytes_tail(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + data.len().div_ceil(32) * 32);
    out.extend_from_slice(&word_u(data.len() as u128));
    out.extend_from_slice(data);
    let pad = data.len().div_ceil(32) * 32 - data.len();
    out.extend_from_slice(&vec![0u8; pad]);
    out
}

/// `CitrateECDSAValidator` install data: owner(20) ++ source(1).
pub fn ecdsa_install_data(owner: &str, source: u8) -> Result<Vec<u8>, AaError> {
    let o = hex_to_bytes(owner)?;
    if o.len() != 20 {
        return Err(AaError::Invalid("owner must be a 20-byte address".into()));
    }
    let mut out = o;
    out.push(source);
    Ok(out)
}

/// Kernel `initialize(bytes21,address,bytes,bytes,bytes[])` calldata
/// for the WP-8 shape (hook = 0, hookData = "", initConfig = []).
pub fn encode_initialize(root_validator: &str, validator_data: &[u8]) -> Result<Vec<u8>, AaError> {
    let v = hex_to_bytes(root_validator)?;
    if v.len() != 20 {
        return Err(AaError::Invalid("validator must be 20 bytes".into()));
    }
    // bytes21 = 0x01 (VALIDATION_TYPE_VALIDATOR) ++ validator, left-aligned.
    let mut validation_id = [0u8; 32];
    validation_id[0] = 0x01;
    validation_id[1..21].copy_from_slice(&v);

    let v_tail = bytes_tail(validator_data);
    let hook_data_offset = 5 * 32 + v_tail.len();
    let init_config_offset = hook_data_offset + 32;

    let mut out = Vec::new();
    out.extend_from_slice(&hex_to_bytes(SEL_INITIALIZE)?);
    out.extend_from_slice(&validation_id);
    out.extend_from_slice(&[0u8; 32]); // hook = address(0)
    out.extend_from_slice(&word_u(5 * 32)); // validatorData offset
    out.extend_from_slice(&word_u(hook_data_offset as u128));
    out.extend_from_slice(&word_u(init_config_offset as u128));
    out.extend_from_slice(&v_tail);
    out.extend_from_slice(&word_u(0)); // empty hookData
    out.extend_from_slice(&word_u(0)); // empty initConfig array
    Ok(out)
}

/// `deployFor(bytes32,address,bytes,uint256,bytes)` calldata.
pub fn encode_deploy_for(
    user_id: &[u8; 32],
    initial_validator: &str,
    init_data: &[u8],
    expires_at: u64,
    signature: &[u8],
) -> Result<Vec<u8>, AaError> {
    let init_tail = bytes_tail(init_data);
    let head_size = 5 * 32;
    let mut out = Vec::new();
    out.extend_from_slice(&hex_to_bytes(SEL_DEPLOY_FOR)?);
    out.extend_from_slice(user_id);
    out.extend_from_slice(&word_addr(initial_validator)?);
    out.extend_from_slice(&word_u(head_size as u128));
    out.extend_from_slice(&word_u64(expires_at));
    out.extend_from_slice(&word_u((head_size + init_tail.len()) as u128));
    out.extend_from_slice(&init_tail);
    out.extend_from_slice(&bytes_tail(signature));
    Ok(out)
}

/// ERC-7579 `execute(bytes32,bytes)` — single call (callType 0x00).
pub fn encode_execute_single(to: &str, value: u128, data: &[u8]) -> Result<Vec<u8>, AaError> {
    let to_bytes = hex_to_bytes(to)?;
    if to_bytes.len() != 20 {
        return Err(AaError::Invalid("target must be 20 bytes".into()));
    }
    let mut packed = Vec::with_capacity(20 + 32 + data.len());
    packed.extend_from_slice(&to_bytes);
    packed.extend_from_slice(&word_u(value));
    packed.extend_from_slice(data);

    let mut out = Vec::new();
    out.extend_from_slice(&hex_to_bytes(SEL_EXECUTE)?);
    out.extend_from_slice(&[0u8; 32]); // execMode: single, default
    out.extend_from_slice(&word_u(64)); // executionCalldata offset
    out.extend_from_slice(&bytes_tail(&packed));
    Ok(out)
}

/// Pack two 128-bit halves into one bytes32 (`hi ++ lo`) — the v0.7
/// `accountGasLimits` (verification ++ call) / `gasFees`
/// (maxPriority ++ maxFee) shape.
pub fn pack_pair128(hi: u128, lo: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[..16].copy_from_slice(&hi.to_be_bytes());
    w[16..].copy_from_slice(&lo.to_be_bytes());
    w
}

/// `paymaster(20) ++ verifGas(16) ++ postOpGas(16) ++ category(1)` —
/// the CitratePaymaster tag sits at byte offset 52 (`PMD_TAG_OFFSET`).
pub fn pack_paymaster_and_data(
    paymaster: &str,
    verification_gas: u128,
    post_op_gas: u128,
    category: u8,
) -> Result<Vec<u8>, AaError> {
    let p = hex_to_bytes(paymaster)?;
    if p.len() != 20 {
        return Err(AaError::Invalid("paymaster must be 20 bytes".into()));
    }
    let mut out = Vec::with_capacity(53);
    out.extend_from_slice(&p);
    out.extend_from_slice(&verification_gas.to_be_bytes());
    out.extend_from_slice(&post_op_gas.to_be_bytes());
    out.push(category);
    Ok(out)
}

/// The fields EntryPoint v0.7's `getUserOpHash` commits to.
pub struct PackedUserOp<'a> {
    pub sender: &'a str,
    pub nonce: u128,
    pub init_code: &'a [u8],
    pub call_data: &'a [u8],
    pub account_gas_limits: [u8; 32],
    pub pre_verification_gas: u128,
    pub gas_fees: [u8; 32],
    pub paymaster_and_data: &'a [u8],
}

/// EntryPoint v0.7 `getUserOpHash` — all dynamic fields pre-hashed, so
/// the ABI encoding degenerates to fixed 32-byte words. Verified
/// against the LIVE EntryPoint via eth_call (vector in the tests).
pub fn get_user_op_hash(
    op: &PackedUserOp<'_>,
    entry_point: &str,
    chain_id: u64,
) -> Result<[u8; 32], AaError> {
    let mut inner = Vec::with_capacity(8 * 32);
    inner.extend_from_slice(&word_addr(op.sender)?);
    inner.extend_from_slice(&word_u(op.nonce));
    inner.extend_from_slice(&keccak(op.init_code));
    inner.extend_from_slice(&keccak(op.call_data));
    inner.extend_from_slice(&op.account_gas_limits);
    inner.extend_from_slice(&word_u(op.pre_verification_gas));
    inner.extend_from_slice(&op.gas_fees);
    inner.extend_from_slice(&keccak(op.paymaster_and_data));
    let inner_hash = keccak(&inner);

    let mut outer = Vec::with_capacity(3 * 32);
    outer.extend_from_slice(&inner_hash);
    outer.extend_from_slice(&word_addr(entry_point)?);
    outer.extend_from_slice(&word_u64(chain_id));
    Ok(keccak(&outer))
}

/// Calldata for `EntryPoint.getNonce(sender, 0)` (root-validator key).
pub fn get_nonce_calldata(sender: &str) -> Result<Vec<u8>, AaError> {
    let mut out = Vec::with_capacity(4 + 64);
    out.extend_from_slice(&hex_to_bytes(SEL_GET_NONCE)?);
    out.extend_from_slice(&word_addr(sender)?);
    out.extend_from_slice(&[0u8; 32]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vector sources (no mocks):
    //  - prediction: LIVE chain-40204 eth_call captures, 2026-06-11
    //    (cast call $FACTORY 'predictAddress(bytes32)' …)
    //  - calldata: generated from citrate-sdk-js src/aa (the canonical
    //    TS implementation) so the encoders cannot drift
    //  - userOpHash: LIVE EntryPoint v0.7 eth_call capture

    const UUID: &str = "0d1f02f1-1f5a-4f5e-9c2e-7b8d1a2b3c4d";
    const UUID_USER_ID: &str = "23691dc9a1d9d7ffa4787edf129321063826c584f406598e645141dba9db32d8";
    const OWNER: &str = "0x8ba1f109551bd432803012645ac136ddd64dba72";

    fn expect_hex(bytes: &[u8]) -> String {
        hex::encode(bytes)
    }

    #[test]
    fn uuid_to_user_id_matches_the_cross_surface_mapping() {
        let id = uuid_to_user_id(UUID).expect("valid uuid");
        assert_eq!(expect_hex(&id), UUID_USER_ID);
        let upper = uuid_to_user_id(&UUID.to_uppercase()).expect("case-normalized");
        assert_eq!(upper, id);
        assert!(uuid_to_user_id("nope").is_err());
    }

    #[test]
    fn account_id_covers_all_three_shapes() {
        let raw = format!("0x{}", "ab".repeat(32));
        assert_eq!(
            expect_hex(&account_id_to_user_id(&raw).expect("raw 32-byte")),
            "ab".repeat(32)
        );
        let padded = account_id_to_user_id(OWNER).expect("EOA pads");
        assert_eq!(
            expect_hex(&padded),
            format!("{}{}", "0".repeat(24), strip0x(OWNER))
        );
        assert_eq!(
            expect_hex(&account_id_to_user_id(UUID).expect("uuid hashes")),
            UUID_USER_ID
        );
        assert!(account_id_to_user_id("junk").is_err());
    }

    #[test]
    fn prediction_matches_the_live_factory() {
        let mut user1 = [0u8; 32];
        user1.copy_from_slice(&hex::decode("11".repeat(32)).expect("hex"));
        let got = predict_wallet_address(addresses::FACTORY, addresses::WALLET_IMPL, &user1)
            .expect("prediction");
        // cast call 0xd951…FD57 'predictAddress(bytes32)' 0x1111…11
        assert_eq!(got, "0x5ce327300221659b66323dc344c2275a7da756ff");

        let uuid_id = uuid_to_user_id(UUID).expect("uuid");
        let got2 = predict_wallet_address(addresses::FACTORY, addresses::WALLET_IMPL, &uuid_id)
            .expect("prediction");
        assert_eq!(got2, "0x05d25d894e88b288f3f7508ce6523d79dee5de28");
    }

    #[test]
    fn initialize_calldata_matches_the_sdk_vector() {
        let install =
            ecdsa_install_data(OWNER, 2 /* wallet-extension source used in the shared vector */)
                .expect("install data");
        assert_eq!(expect_hex(&install), format!("{}02", strip0x(OWNER)));
        let calldata =
            encode_initialize(addresses::ECDSA_VALIDATOR, &install).expect("initialize");
        let expected = "3c3b752b01d2d35421379ae5b461e216bfcdd1b7e6a64bbc400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000158ba1f109551bd432803012645ac136ddd64dba7202000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(expect_hex(&calldata), expected);
    }

    #[test]
    fn deploy_for_calldata_is_well_formed() {
        let mut user = [0u8; 32];
        user.copy_from_slice(&hex::decode("11".repeat(32)).expect("hex"));
        let install = ecdsa_install_data(OWNER, ECDSA_SOURCE_GUI_NATIVE).expect("install");
        let init = encode_initialize(addresses::ECDSA_VALIDATOR, &install).expect("init");
        let sig = vec![0x22u8; 65];
        let calldata = encode_deploy_for(&user, addresses::ECDSA_VALIDATOR, &init, 1_780_000_000, &sig)
            .expect("deployFor");
        let h = expect_hex(&calldata);
        assert!(h.starts_with("89ebe13b"));
        assert!(h.contains(&"11".repeat(32)));
        assert!(h.contains(strip0x(addresses::ECDSA_VALIDATOR)));
        assert!(h.contains(&"22".repeat(65)));
    }

    #[test]
    fn execute_single_matches_the_sdk_vector() {
        let calldata = encode_execute_single(
            "0x000000000000000000000000000000000000dead",
            1,
            &hex::decode("abcdef").expect("hex"),
        )
        .expect("execute");
        // The exact citrate-sdk-js encodeExecuteSingle vector.
        let exact = format!(
            "e9ae5c53{}{}{}{}{}{}",
            "00".repeat(32),
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000037",
            "000000000000000000000000000000000000dead",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "abcdef000000000000000000"
        );
        assert_eq!(expect_hex(&calldata), exact);
    }

    #[test]
    fn user_op_hash_matches_the_live_entrypoint() {
        let op = PackedUserOp {
            sender: "0x5ce327300221659b66323dc344c2275a7da756ff",
            nonce: 0,
            init_code: &[],
            call_data: &hex::decode("deadbeef").expect("hex"),
            account_gas_limits: pack_pair128(150_000, 100_000),
            pre_verification_gas: 50_000,
            gas_fees: pack_pair128(1_000_000_000, 2_000_000_000),
            paymaster_and_data: &[],
        };
        let h = get_user_op_hash(&op, addresses::ENTRY_POINT, addresses::CHAIN_ID)
            .expect("hash");
        assert_eq!(
            expect_hex(&h),
            "5369c256d308e61a1fe8b15aaaa7709fee6b8edd46b5c452a9bc5ec8965629fd"
        );
    }

    #[test]
    fn paymaster_data_puts_the_category_at_offset_52() {
        let pmd = pack_paymaster_and_data(addresses::PAYMASTER, 60_000, 40_000, 2)
            .expect("paymaster data");
        assert_eq!(pmd.len(), 53);
        assert_eq!(pmd[52], 2);
    }

    #[test]
    fn get_nonce_calldata_targets_root_key() {
        let data = get_nonce_calldata("0x5ce327300221659b66323dc344c2275a7da756ff")
            .expect("calldata");
        assert_eq!(hex::encode(&data[..4]), "35567e1a");
        assert_eq!(data.len(), 4 + 64);
    }
}
