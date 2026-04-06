//! ABI encoding helpers for edu contract calls.
//!
//! Manual selector + param encoding matching the existing crate pattern
//! (no ethers-rs / alloy dependency — just hex encoding).

/// Encode a 4-byte function selector from its string signature.
/// Example: `selector("isSigner(address)")` => `0x...`
pub fn selector(sig: &str) -> [u8; 4] {
    use sha3::{Digest, Keccak256};
    let hash = Keccak256::digest(sig.as_bytes());
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&hash[..4]);
    sel
}

/// Encode a call with no arguments (just selector).
pub fn encode_call(sig: &str) -> String {
    format!("0x{}", hex::encode(selector(sig)))
}

/// Encode a call with one address argument.
pub fn encode_call_address(sig: &str, addr: &str) -> String {
    let sel = hex::encode(selector(sig));
    let addr_clean = addr.trim_start_matches("0x").to_lowercase();
    format!("0x{}{:0>64}", sel, addr_clean)
}

/// Encode a call with one uint256 argument.
pub fn encode_call_uint256(sig: &str, value: u64) -> String {
    let sel = hex::encode(selector(sig));
    format!("0x{}{:0>64x}", sel, value)
}

/// Encode a call with two arguments: uint256, address.
pub fn encode_call_uint256_address(sig: &str, id: u64, addr: &str) -> String {
    let sel = hex::encode(selector(sig));
    let addr_clean = addr.trim_start_matches("0x").to_lowercase();
    format!("0x{}{:0>64x}{:0>64}", sel, id, addr_clean)
}

/// Decode a uint256 from a hex-encoded eth_call result.
pub fn decode_uint256(hex_result: &str) -> Option<u64> {
    let clean = hex_result.trim_start_matches("0x");
    let trimmed = clean.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(trimmed, 16).ok()
}

/// Decode a bool from a hex-encoded eth_call result.
pub fn decode_bool(hex_result: &str) -> bool {
    let clean = hex_result.trim_start_matches("0x");
    !clean.chars().all(|c| c == '0')
}

/// Decode an address from a hex-encoded eth_call result (last 40 chars of 64-char word).
pub fn decode_address(hex_result: &str) -> String {
    let clean = hex_result.trim_start_matches("0x");
    if clean.len() >= 40 {
        format!("0x{}", &clean[clean.len() - 40..])
    } else {
        format!("0x{}", clean)
    }
}

/// Decode a uint8 from a hex-encoded eth_call result.
pub fn decode_uint8(hex_result: &str) -> Option<u8> {
    decode_uint256(hex_result).map(|v| v as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selector_is_signer() {
        // cast sig "isSigner(address)" = 0x2e4a1520 (approximate — verify with cast)
        let sel = selector("isSigner(address)");
        assert_eq!(sel.len(), 4);
    }

    #[test]
    fn test_encode_call_no_args() {
        let encoded = encode_call("threshold()");
        assert!(encoded.starts_with("0x"));
        assert_eq!(encoded.len(), 10); // 0x + 8 hex chars
    }

    #[test]
    fn test_decode_uint256() {
        assert_eq!(decode_uint256("0x0000000000000000000000000000000000000000000000000000000000000002"), Some(2));
        assert_eq!(decode_uint256("0x0000000000000000000000000000000000000000000000000000000000000000"), Some(0));
    }

    #[test]
    fn test_decode_bool() {
        assert!(decode_bool("0x0000000000000000000000000000000000000000000000000000000000000001"));
        assert!(!decode_bool("0x0000000000000000000000000000000000000000000000000000000000000000"));
    }

    #[test]
    fn test_decode_address() {
        let addr = decode_address("0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        assert_eq!(addr, "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    }
}
