//! Calldata method decoder for the DAG explorer's transaction modal.
//!
//! Given a transaction's raw `data` field, look up the human-readable
//! method name by matching the first 4 bytes (the function selector,
//! `keccak256(canonical_signature)[..4]`).
//!
//! The registry is intentionally curated — not exhaustive. We cover:
//! - core ERC-20 + ERC-721 + ERC-1155 writes (transfers, approvals)
//! - the BFR write surface used by the audit + provenance + governance
//!   stack (record / anchor / attest / fire / recordStep / record_signers)
//! - the BFR read surface from `citrate_rbac_bindings::live::selectors`
//!   that operators routinely call via cast/curl
//!
//! Anything else falls through to `Unknown`. The modal still renders
//! the raw 4-byte prefix + the full input hex so the operator can
//! cross-reference manually.

use sha3::{Digest, Keccak256};
use std::sync::OnceLock;

/// Result of decoding the first 4 bytes of a transaction's `data` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorMatch {
    /// `data` is empty — this is a value-transfer (or a no-op call).
    Empty,
    /// `data` is shorter than 4 bytes or not valid hex.
    Malformed,
    /// 4-byte selector matched a known canonical signature.
    Known {
        canonical: &'static str,
        selector: [u8; 4],
    },
    /// 4-byte selector did not match any entry in the registry.
    Unknown { selector: [u8; 4] },
}

impl SelectorMatch {
    /// Short display label for the modal.
    pub fn label(&self) -> String {
        match self {
            SelectorMatch::Empty => "Transfer".to_string(),
            SelectorMatch::Malformed => "Malformed".to_string(),
            SelectorMatch::Known { canonical, .. } => (*canonical).to_string(),
            SelectorMatch::Unknown { selector } => {
                format!("0x{} (unknown)", hex::encode(selector))
            }
        }
    }
}

fn compute(sig: &str) -> [u8; 4] {
    let mut hasher = Keccak256::new();
    hasher.update(sig.as_bytes());
    let h = hasher.finalize();
    let mut out = [0u8; 4];
    out.copy_from_slice(&h[..4]);
    out
}

/// Canonical ABI signatures the modal knows about. Order is not
/// significant — first match wins, but selectors are unique to a
/// signature so order doesn't change semantics. Add to this list
/// when a new BFR write or read enters the operator workflow.
const CANONICAL_SIGNATURES: &[&str] = &[
    // ERC-20 writes
    "transfer(address,uint256)",
    "approve(address,uint256)",
    "transferFrom(address,address,uint256)",
    "increaseAllowance(address,uint256)",
    "decreaseAllowance(address,uint256)",
    // ERC-721 / ERC-1155 writes
    "safeTransferFrom(address,address,uint256)",
    "safeTransferFrom(address,address,uint256,bytes)",
    "setApprovalForAll(address,bool)",
    // BFR audit + governance writes
    "record(bytes32,bytes32,bytes32,bytes32,uint8,string,string,bytes32,string)",
    "requestElevation(bytes32,bytes32,bytes32,uint32,bytes32,bytes,string)",
    "revoke(bytes32,bytes32,string,bytes32)",
    "anchor(uint8,bytes32,bytes32,bytes32,bytes32,string,uint64)",
    "fire(bytes32,bytes32,uint8,string,string,bytes32)",
    "attest(bytes32,bytes32,uint8,string,string,bytes32)",
    "signExport(bytes32,bytes,bytes32)",
    // BFR provenance writes
    "recordStep(bytes32,uint8,bytes32,bytes32,bytes32,string)",
    // BFR cross-org + role writes
    "record(bytes32,bytes32)",
    "sign(bytes32,bytes32)",
    "registerModel(bytes32,bytes32)",
    // ModelRegistry precompile write — the GUI's own model-publish tx
    // (app_binder::encode_register_model_calldata). GUI_NATIVE-2026-05-31-004:
    // the encoder used this signature while the decoder only knew the BFR
    // bytes32,bytes32 variant, so operators couldn't decode their own tx.
    "registerModel(bytes32,string)",
    // BFR read selectors (top operator queries — full set in
    // citrate_rbac_bindings::live::selectors but these are the
    // ones operators commonly hit by hand)
    "getPath(bytes32)",
    "getNode(bytes32)",
    "latestGrant(bytes32,bytes32)",
    "getClearance(bytes32)",
    "latestByTenant(bytes32,uint256)",
    "getDecision(bytes32)",
    "hasOpenContradiction(bytes32)",
    "bySubject(bytes32)",
    "getEnvelope(bytes32)",
    "lineage(bytes32)",
    "verifyChain(bytes32)",
    "byScope(bytes32)",
    "byState(uint8)",
    "poolsByScope(bytes32)",
    "getPool(uint256)",
    "allContracts()",
    "contractCount()",
    "getApp(bytes32)",
    "getContract(address)",
    "getRelease(bytes32)",
    "getBundle(bytes32)",
    "firingCount()",
    "allFirings()",
];

/// One-time-computed registry of (canonical signature, selector bytes).
fn registry() -> &'static Vec<(&'static str, [u8; 4])> {
    static CELL: OnceLock<Vec<(&'static str, [u8; 4])>> = OnceLock::new();
    CELL.get_or_init(|| {
        CANONICAL_SIGNATURES
            .iter()
            .map(|sig| (*sig, compute(sig)))
            .collect()
    })
}

/// Decode the first 4 bytes of a transaction's `data` field.
///
/// Accepts either raw hex (`"a9059cbb..."`) or `0x`-prefixed hex
/// (`"0xa9059cbb..."`). Empty input is treated as a value transfer.
pub fn decode_selector(input_hex: &str) -> SelectorMatch {
    let trimmed = input_hex.trim();
    if trimmed.is_empty() {
        return SelectorMatch::Empty;
    }
    let stripped = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if stripped.is_empty() {
        return SelectorMatch::Empty;
    }
    if stripped.len() < 8 {
        return SelectorMatch::Malformed;
    }
    let head = &stripped[..8];
    let bytes = match hex::decode(head) {
        Ok(b) => b,
        Err(_) => return SelectorMatch::Malformed,
    };
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&bytes);

    for (canonical, sel) in registry().iter() {
        if *sel == selector {
            return SelectorMatch::Known {
                canonical,
                selector,
            };
        }
    }
    SelectorMatch::Unknown { selector }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_transfer() {
        assert_eq!(decode_selector(""), SelectorMatch::Empty);
        assert_eq!(decode_selector("   "), SelectorMatch::Empty);
        assert_eq!(decode_selector("0x"), SelectorMatch::Empty);
    }

    #[test]
    fn malformed_input_is_caught() {
        assert_eq!(decode_selector("0xabc"), SelectorMatch::Malformed);
        assert_eq!(decode_selector("0xgggggggg"), SelectorMatch::Malformed);
        assert_eq!(decode_selector("not-hex"), SelectorMatch::Malformed);
    }

    #[test]
    fn erc20_transfer_resolves() {
        // 0xa9059cbb = keccak256("transfer(address,uint256)")[..4]
        let result = decode_selector(
            "0xa9059cbb000000000000000000000000abcd000000000000000000000000000000000000",
        );
        match result {
            SelectorMatch::Known { canonical, .. } => {
                assert_eq!(canonical, "transfer(address,uint256)");
            }
            other => panic!("expected Known(transfer), got {:?}", other),
        }
    }

    #[test]
    fn bfr_record_resolves() {
        // record(bytes32,bytes32,bytes32,bytes32,uint8,string,string,bytes32,string)
        let sel =
            compute("record(bytes32,bytes32,bytes32,bytes32,uint8,string,string,bytes32,string)");
        let hex_input = format!("0x{}{}", hex::encode(sel), "00".repeat(32));
        match decode_selector(&hex_input) {
            SelectorMatch::Known { canonical, .. } => {
                assert!(canonical.starts_with("record("));
            }
            other => panic!("expected Known(record), got {:?}", other),
        }
    }

    #[test]
    fn bfr_anchor_resolves() {
        let sel = compute("anchor(uint8,bytes32,bytes32,bytes32,bytes32,string,uint64)");
        let hex_input = format!("0x{}", hex::encode(sel));
        match decode_selector(&hex_input) {
            SelectorMatch::Known { canonical, .. } => {
                assert!(canonical.starts_with("anchor("));
            }
            other => panic!("expected Known(anchor), got {:?}", other),
        }
    }

    /// GUI_NATIVE-2026-05-31-004 (WP 6.4b): the GUI's own model-publish tx is
    /// built with `registerModel(bytes32,string)` (app_binder) — the decoder
    /// modal must recognize it, not just the BFR `registerModel(bytes32,bytes32)`.
    #[test]
    fn model_registry_register_model_resolves() {
        let sel = compute("registerModel(bytes32,string)");
        let hex_input = format!("0x{}{}", hex::encode(sel), "00".repeat(64));
        match decode_selector(&hex_input) {
            SelectorMatch::Known { canonical, .. } => {
                assert_eq!(canonical, "registerModel(bytes32,string)");
            }
            other => panic!(
                "expected Known(registerModel(bytes32,string)), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn unknown_selector_passes_through_with_bytes() {
        // 0xdeadbeef is not a real selector for anything in our registry.
        match decode_selector("0xdeadbeef00") {
            SelectorMatch::Unknown { selector } => {
                assert_eq!(selector, [0xde, 0xad, 0xbe, 0xef]);
            }
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn label_renders_useful_text() {
        assert_eq!(SelectorMatch::Empty.label(), "Transfer");
        assert_eq!(
            SelectorMatch::Known {
                canonical: "transfer(address,uint256)",
                selector: [0xa9, 0x05, 0x9c, 0xbb]
            }
            .label(),
            "transfer(address,uint256)"
        );
        assert_eq!(
            SelectorMatch::Unknown {
                selector: [0xde, 0xad, 0xbe, 0xef]
            }
            .label(),
            "0xdeadbeef (unknown)"
        );
    }

    #[test]
    fn selectors_are_unique_in_registry() {
        let r = registry();
        let mut seen: std::collections::HashSet<[u8; 4]> = std::collections::HashSet::new();
        for (canonical, sel) in r.iter() {
            assert!(
                seen.insert(*sel),
                "duplicate selector for {}: 0x{}",
                canonical,
                hex::encode(sel)
            );
        }
    }
}
