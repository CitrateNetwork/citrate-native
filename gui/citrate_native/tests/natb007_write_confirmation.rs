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

/// Stricter shape check than [`assert_all_gated`]: the nearest gate above each
/// contract write must be a plain `if !confirm_tx_intent(` (so a `false && …`
/// edit or a discarded result fails), and a `return` must sit between the gate
/// and the write (the declined branch cannot fall through to the broadcast).
fn strict_violations(text: &str) -> (usize, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let needle = needle();
    let guard = format!("confirm_tx{}", "_intent(");
    let mut sites = 0usize;
    let mut bad = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&needle) {
            continue;
        }
        sites += 1;
        let lo = i.saturating_sub(WINDOW);
        let Some(g) = (lo..i).rev().find(|&j| lines[j].contains(&guard)) else {
            bad.push(format!("{}: no gate", i + 1));
            continue;
        };
        let gate = lines[g].trim();
        let plain = gate.starts_with(&format!("if !{guard}"))
            || gate.starts_with(&format!("if !crate::{guard}"));
        let returns = lines[g..i].iter().any(|l| l.trim() == "return;");
        if !plain || !returns {
            bad.push(format!(
                "{}: gate `{gate}` (returns before write: {returns})",
                i + 1
            ));
        }
    }
    (sites, bad)
}

fn assert_strictly_gated(rel: &str) {
    let (sites, bad) = strict_violations(&src(rel));
    assert!(sites > 0, "{rel}: no contract-write call sites found");
    assert!(
        bad.is_empty(),
        "{rel}: weakly gated contract writes:\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_contract_write_gate_is_a_plain_refusal_in_main() {
    assert_strictly_gated("src/main.rs");
}

#[test]
fn every_contract_write_gate_is_a_plain_refusal_in_app_binder() {
    assert_strictly_gated("src/app_binder.rs");
}

#[test]
fn strict_gate_check_negative_controls() {
    let write = needle();
    let gate = format!("confirm_tx{}", "_intent(");
    let ok = format!("if !{gate}a).await {{\n    return;\n}}\nwallet{write}x)\n");
    assert!(
        strict_violations(&ok).1.is_empty(),
        "control shape must pass"
    );
    let disabled = format!("if false && !{gate}a).await {{\n    return;\n}}\nwallet{write}x)\n");
    assert_eq!(strict_violations(&disabled).1.len(), 1);
    let discarded = format!("let _ = {gate}a).await;\nwallet{write}x)\n");
    assert_eq!(strict_violations(&discarded).1.len(), 1);
    let no_return = format!("if !{gate}a).await {{\n    log();\n}}\nwallet{write}x)\n");
    assert_eq!(strict_violations(&no_return).1.len(), 1);
}
