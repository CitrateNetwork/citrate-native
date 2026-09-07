//! NAT-B-007 tripwire — every user-initiated contract-write call site must be
//! lexically preceded by a `confirm_tx_intent(...)` submission to the shared
//! `PendingApprovalStore`, so no signing path can broadcast calldata without a
//! decoded target/method/amount confirmation surface.
//!
//! The audit found six `send_transaction_with_data` call sites in `main.rs` and
//! one in `app_binder.rs` that built calldata and broadcast on a single button
//! click with no confirmation at all. This test fails (RED) if any such call
//! site reappears without an approval gate in front of it, and is the source of
//! truth the remediation restored to GREEN.
//!
//! It is a pure source scan — it reads the crate's own `.rs` files from
//! `CARGO_MANIFEST_DIR` and never links the GUI, so it runs fast in CI.

use std::fs;
use std::path::PathBuf;

/// How many lines above a call site we allow the approval gate to sit.
const WINDOW: usize = 45;

fn src(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Assembled at runtime so this test file does not itself contain the literal
/// needle (it would otherwise match were the scan ever pointed at this file).
fn needle() -> String {
    format!(".send_transaction{}", "_with_data(")
}

fn assert_all_gated(rel: &str) {
    let text = src(rel);
    let lines: Vec<&str> = text.lines().collect();
    let needle = needle();
    let guard = format!("confirm_tx{}", "_intent(");
    let mut sites = 0usize;
    let mut ungated: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(&needle) {
            sites += 1;
            let lo = i.saturating_sub(WINDOW);
            let gated = lines[lo..i].iter().any(|w| w.contains(&guard));
            if !gated {
                ungated.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        sites > 0,
        "{rel}: expected at least one contract-write call site; found none \
         (did the file move or the API get renamed?)"
    );
    assert!(
        ungated.is_empty(),
        "NAT-B-007: contract writes broadcast with no confirmation surface:\n{}",
        ungated.join("\n")
    );
}

#[test]
fn every_contract_write_is_confirmation_gated_in_main() {
    assert_all_gated("src/main.rs");
}

#[test]
fn every_contract_write_is_confirmation_gated_in_app_binder() {
    assert_all_gated("src/app_binder.rs");
}
