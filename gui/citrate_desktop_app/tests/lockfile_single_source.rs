//! The workspace must build against ONE copy of each git-sourced crate,
//! pinned by an immutable `rev`: no `branch=` git source in `Cargo.lock`, and
//! no crate name resolved from two different sources. A second copy of a
//! chain crate (for example the wallet core) pulled in transitively by
//! branch would mean the reviewed rev is not the only code that ships.
//!
//! `scripts/ci/check-chain-dep-pins.sh` checks the same thing, but no workflow
//! runs it; this test runs with `cargo test`, which CI does run. It reads the
//! committed lock file only (no network, no cargo invocation).

use std::collections::BTreeMap;

/// Returns the violations found in a `Cargo.lock` body.
fn lock_violations(lock: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = \"") {
            name = Some(n.trim_end_matches('"').to_string());
        } else if let Some(s) = line.strip_prefix("source = \"") {
            let s = s.trim_end_matches('"').to_string();
            if s.starts_with("git+") && s.contains("branch=") {
                out.push(format!("branch-tracking git source: {s}"));
            }
            if let Some(n) = &name {
                let list = sources.entry(n.clone()).or_default();
                if !list.contains(&s) {
                    list.push(s);
                }
            }
        } else if line.starts_with("[[package]]") {
            name = None;
        }
    }
    for (n, srcs) in sources {
        let git: Vec<&String> = srcs.iter().filter(|s| s.starts_with("git+")).collect();
        if git.len() > 1 {
            out.push(format!(
                "{n} resolved from {} git sources: {git:?}",
                git.len()
            ));
        }
    }
    out
}

#[test]
fn lockfile_has_one_rev_pinned_source_per_git_crate() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        lock.contains("name = \"citrate-wallet-core\""),
        "sanity: the lock must contain the wallet core"
    );
    let v = lock_violations(&lock);
    assert!(
        v.is_empty(),
        "Cargo.lock git-source violations:\n{}",
        v.join("\n")
    );
}

#[test]
fn lock_violations_negative_controls() {
    let dup = "[[package]]\nname = \"citrate-wallet-core\"\nversion = \"0.4.0\"\nsource = \"git+https://example/citrate-chain?rev=aaa#aaa\"\n\n\
               [[package]]\nname = \"citrate-wallet-core\"\nversion = \"0.4.0\"\nsource = \"git+https://example/citrate-chain?branch=main#bbb\"\n";
    let v = lock_violations(dup);
    assert!(v.iter().any(|m| m.contains("branch-tracking")), "{v:?}");
    assert!(v.iter().any(|m| m.contains("2 git sources")), "{v:?}");

    let ok = "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
              [[package]]\nname = \"citrate-wallet-core\"\nversion = \"0.4.0\"\nsource = \"git+https://example/citrate-chain?rev=aaa#aaa\"\n";
    assert!(lock_violations(ok).is_empty());
}
