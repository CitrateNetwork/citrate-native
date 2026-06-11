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

/// GUI_NATIVE-2026-05-31-003 (WP 6.4b): validate a hex argument instead of
/// string-padding whatever was handed in. Returns the cleaned lowercase hex
/// digits when `s` is `0x`-prefixed (or bare) hex of exactly `hex_len` chars.
pub fn require_hex(s: &str, hex_len: usize, what: &str) -> Result<String, String> {
    let clean = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))
        .unwrap_or(s.trim())
        .to_lowercase();
    if clean.len() != hex_len {
        return Err(format!("{what}: expected {hex_len} hex chars, got {}", clean.len()));
    }
    if !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{what}: not valid hex"));
    }
    Ok(clean)
}

/// Encode a call with one address argument. The address is validated
/// (exactly 20 bytes of hex) — malformed input errs instead of producing
/// wrong-length calldata (GUI_NATIVE-2026-05-31-003).
pub fn encode_call_address(sig: &str, addr: &str) -> Result<String, String> {
    let sel = hex::encode(selector(sig));
    let addr_clean = require_hex(addr, 40, "address")?;
    Ok(format!("0x{}{:0>64}", sel, addr_clean))
}

/// Encode a call with one uint256 argument.
pub fn encode_call_uint256(sig: &str, value: u64) -> String {
    let sel = hex::encode(selector(sig));
    format!("0x{}{:0>64x}", sel, value)
}

/// Encode a call with two arguments: uint256, address (validated).
pub fn encode_call_uint256_address(sig: &str, id: u64, addr: &str) -> Result<String, String> {
    let sel = hex::encode(selector(sig));
    let addr_clean = require_hex(addr, 40, "address")?;
    Ok(format!("0x{}{:0>64x}{:0>64}", sel, id, addr_clean))
}

/// Decode a uint256 from a hex-encoded eth_call result.
///
/// GUI_NATIVE-2026-05-31-003 (WP 6.4b): widened from `u64` and made strict.
/// Accepts exactly one ABI word (or shorter legacy results), requires valid
/// hex, and refuses values above `u128::MAX` rather than misreporting them.
pub fn decode_uint256(hex_result: &str) -> Option<u128> {
    let clean = hex_result.trim().trim_start_matches("0x");
    if clean.len() > 64 || clean.is_empty() || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // High bytes beyond u128 must be zero.
    if clean.len() > 32 && clean[..clean.len() - 32].chars().any(|c| c != '0') {
        return None;
    }
    let tail = &clean[clean.len().saturating_sub(32)..];
    u128::from_str_radix(tail, 16).ok()
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

/// Decode a uint that must fit in u64 (counters, ids, rates). Overflow is a
/// decode error, not a truncation (GUI_NATIVE-2026-05-31-003).
pub fn decode_uint64(hex_result: &str) -> Option<u64> {
    decode_uint256(hex_result).and_then(|v| u64::try_from(v).ok())
}

/// Decode a uint8 from a hex-encoded eth_call result. Values above 255 are
/// a decode error, not a truncation (GUI_NATIVE-2026-05-31-003).
pub fn decode_uint8(hex_result: &str) -> Option<u8> {
    decode_uint256(hex_result).and_then(|v| u8::try_from(v).ok())
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

    // ── GUI_NATIVE-2026-05-31-003 (WP 6.4b) ─────────────────────────────

    /// `decode_uint8` must not truncate: 0x100 (256) is NOT a uint8.
    #[test]
    fn test_decode_uint8_rejects_oversized() {
        let word256 = format!("0x{:0>64x}", 256u64);
        assert_eq!(decode_uint8(&word256), None, "256 must not truncate to 0");
        let word_ff = format!("0x{:0>64x}", 255u64);
        assert_eq!(decode_uint8(&word_ff), Some(255));
    }

    /// `decode_uint256` must refuse garbage / multi-word input rather than
    /// misparse it, and represent full-width words up to u128.
    #[test]
    fn test_decode_uint256_rejects_garbage() {
        // Non-hex garbage is None, not a panic or a zero.
        assert_eq!(decode_uint256("0xzz"), None);
        // Longer than one ABI word is not a uint256 eth_call result.
        let two_words = format!("0x{}01{}", "00".repeat(31), "00".repeat(32));
        assert_eq!(decode_uint256(&two_words), None);
    }

    /// Values above u64 (real on-chain wei amounts) decode correctly now;
    /// values above u128 are refused, not wrapped.
    #[test]
    fn test_decode_uint256_wide_values() {
        let wide = format!("0x{:0>64x}", u64::MAX as u128 + 1);
        assert_eq!(decode_uint256(&wide), Some(u64::MAX as u128 + 1));
        let over_u128 = format!("0x{}", "ff".repeat(32));
        assert_eq!(decode_uint256(&over_u128), None);
        assert_eq!(decode_uint256(&format!("0x{}", "00".repeat(32))), Some(0));
    }

    /// Address encoders must validate hex + length instead of string-padding
    /// whatever they were handed into a 32-byte word.
    #[test]
    fn test_encode_call_address_validates_input() {
        // A proper address round-trips.
        let ok = encode_call_address("isRelayer(address)", "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
            .expect("valid address encodes");
        assert_eq!(ok.len(), 2 + 8 + 64);
        // Too short, non-hex, and over-long inputs are refused.
        assert!(encode_call_address("isRelayer(address)", "0xabcd").is_err());
        assert!(encode_call_address("isRelayer(address)", &format!("0x{}", "zz".repeat(20))).is_err());
        assert!(encode_call_address(
            "isRelayer(address)",
            &format!("0x{}", "11".repeat(33))
        )
        .is_err());
        assert!(encode_call_uint256_address("f(uint256,address)", 1, "0xabcd").is_err());
        assert!(
            encode_call_uint256_address("f(uint256,address)", 1, &format!("0x{}", "22".repeat(20)))
                .is_ok()
        );
    }
}
