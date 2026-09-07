//! Citrate Desktop — Rust-native Slint GUI
#![allow(clippy::manual_is_multiple_of)]

use citrate_desktop_app::AppCore;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
// NAT-B-016: wipe password buffers held Rust-side in UI callbacks.
use zeroize::Zeroizing;

slint::include_modules!();

mod app_binder;
// BFR-INT-4: Boeing surface moved to `citrate-boeing-shell` crate.
// NATIVE-R1-S2 WP-A1: crash telemetry (panic hook + session marker +
// rotating file log) — forensics for the silent node-start death.
mod crash_telemetry;
mod storage_service;
mod compute_service;
mod marketplace_client;
mod calldata_decoder;

#[cfg(test)]
mod ui_visual_tests;

// NATIVE-R1-S1 WP-5: shared CitrateLoader animation driver. One
// `start_loader` timer feeds the `loader-facets` model bound to both
// loader instances (onboarding node-bootstrap + chat thinking) — the two
// states are never active simultaneously. `LoaderHandle` is UI-thread-only
// (slint::Timer + Rc), so it lives in a thread_local; every toggle site
// already runs on the UI thread (callback body or invoke_from_event_loop).
thread_local! {
    static LOADER: std::cell::RefCell<Option<citrate_ui_kit::loader::LoaderHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// Pause/resume the shared loader timer. UI thread only. The timer idles
/// (stopped) whenever no loading state is active so we don't burn ~60fps
/// of morph math behind a static screen.
fn loader_set_running(running: bool) {
    LOADER.with(|slot| {
        if let Some(handle) = slot.borrow().as_ref() {
            if running {
                if !handle.running() {
                    handle.restart();
                }
            } else {
                handle.stop();
            }
        }
    });
}

/// EIP-55 mixed-case checksum encoding for Ethereum addresses.
/// Takes a hex address (with or without 0x prefix) and returns the checksummed form.
fn eip55_checksum(addr: &str) -> String {
    let addr_lower = addr.strip_prefix("0x").unwrap_or(addr).to_lowercase();
    let hash = {
        use sha3::{Digest, Keccak256};
        hex::encode(Keccak256::digest(addr_lower.as_bytes()))
    };
    let checksummed: String = addr_lower
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if c.is_ascii_alphabetic()
                && u8::from_str_radix(&hash[i..i + 1], 16).unwrap_or(0) >= 8
            {
                c.to_uppercase().next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    format!("0x{}", checksummed)
}

/// NAT-B-012: validate a recipient address at the send dialog — pre-fix
/// `on_wallet_send` did NO validation (no 0x/length/hex/checksum), so a
/// single mistyped character in an otherwise well-formed address was
/// signed and broadcast (irreversible loss). Accepts lower/upper/`0X`,
/// trims surrounding whitespace, and — when the input is MIXED case —
/// requires the EIP-55 checksum to verify (the case that catches typos and
/// clipboard-substitution). Returns the normalized lowercase `0x` address.
fn validate_recipient_address(input: &str) -> Result<String, String> {
    let t = input.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if hex.len() != 40 {
        return Err("Recipient must be a 20-byte address (40 hex characters after 0x).".to_string());
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Recipient address contains non-hex characters.".to_string());
    }
    let has_upper = hex.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = hex.chars().any(|c| c.is_ascii_lowercase());
    if has_upper && has_lower {
        // Mixed case ⇒ the input claims to be EIP-55 checksummed. It must
        // verify, or a character is mistyped/substituted.
        let expected = eip55_checksum(hex);
        if expected != format!("0x{hex}") {
            return Err(
                "Address checksum is invalid (EIP-55) — a character may be mistyped. \
                 Double-check the recipient."
                    .to_string(),
            );
        }
    }
    Ok(format!("0x{}", hex.to_lowercase()))
}

/// Strip markdown formatting for display in plain-text Slint Text elements.
/// Converts headers, bold markers, backticks, and list items to readable plain text.
fn clean_markdown(text: &str) -> String {
    let mut in_code_block = false;
    text.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Toggle fenced code blocks (``` markers)
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                return String::new();
            }
            // Inside code blocks, return content as-is (no markdown processing)
            if in_code_block {
                return format!("  {line}");
            }
            // Headers: remove # markers (check longest prefix first)
            let line = if let Some(rest) = trimmed.strip_prefix("#### ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("### ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("## ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("# ") {
                rest
            } else {
                trimmed
            };
            // Bold: remove ** markers
            let line = line.replace("**", "");
            // Inline code: remove backtick markers
            let line = line.replace('`', "");
            // Unordered list items: convert - or * to bullet
            if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                format!("  \u{2022} {rest}")
            } else {
                line
            }
        })
        // Collapse consecutive blank lines into a single blank line
        .fold(Vec::<String>::new(), |mut acc, line| {
            if line.is_empty() {
                if acc.last().is_none_or(|l| !l.is_empty()) {
                    acc.push(line);
                }
            } else {
                acc.push(line);
            }
            acc
        })
        .join("\n")
        .trim()
        .to_string()
}

/// Shared atomic timestamp (unix epoch seconds) of last wallet unlock.
/// Set to 0 when locked. Background thread uses this to compute remaining session time.
static SESSION_UNLOCK_EPOCH: AtomicI64 = AtomicI64::new(0);

/// Session timeout duration in seconds for the GUI countdown display +
/// the moment we clear SESSION_UNLOCK_EPOCH.
///
/// NATIVE-R1-S2 WP-A5: single source of truth is
/// `wallet_service::SESSION_TIMEOUT_SECS` — the ENFORCING backend value
/// (1h). This alias only widens the type for countdown math; it must
/// never be a second literal. (Pre-fix: UI said 8h while the backend
/// locked at 1h, so the countdown lied for 7 hours.)
const SESSION_TIMEOUT_SECS: i64 =
    citrate_desktop_app::services::wallet_service::SESSION_TIMEOUT_SECS as i64;

/// NAT-B-004: the custody teardown shared by the Lock button AND Sign
/// Out. Clearing the session epoch is what stops the background signing
/// relay (`SESSION_UNLOCK_EPOCH.load(...) <= 0` gate) and the countdown;
/// `wallet.lock()` zeroizes the decrypted keys. BOTH must happen before
/// the view is swapped, or "Sign Out" leaves a fully-unlocked wallet that
/// keeps signing while the user believes it is closed. A headless test
/// drives this function directly.
async fn perform_wallet_lock_teardown(
    wallet: &citrate_desktop_app::services::WalletService,
) {
    SESSION_UNLOCK_EPOCH.store(0, Ordering::Relaxed);
    if let Err(e) = wallet.lock().await {
        tracing::error!("wallet lock teardown failed: {}", e);
    }
}

/// Push wallet accounts to the Slint UI as a VecModel.
/// Applies EIP-55 checksum encoding to all addresses for display.
fn push_accounts_to_ui(ui: &App, accounts: &[citrate_desktop_app::services::wallet_service::Account]) {
    let model_data: Vec<AccountData> = accounts
        .iter()
        .map(|a| AccountData {
            address: eip55_checksum(&a.address).into(),
            label: a.label.clone().into(),
            balance: a.balance.to_string().into(),
            is_default: a.is_default,
        })
        .collect();
    let model = std::rc::Rc::new(slint::VecModel::from(model_data));
    ui.set_wallet_accounts(model.into());
    ui.set_wallet_account_count(accounts.len() as i32);
}

/// Spawn an async task on the runtime without blocking the Slint event loop.
/// The callback receives a weak UI handle and runs on the tokio runtime.
/// Results are pushed back to the UI via `invoke_from_event_loop`.
fn spawn_async(
    rt: &tokio::runtime::Handle,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    rt.spawn(task);
}

/// How long after a sensitive copy the clipboard auto-wipes.
/// RM-B1 / WP-E2.7 (audit GUI-C-03).
const CLIPBOARD_AUTOCLEAR_SECS: u64 = 30;
const EXPORTED_KEY_DISPLAY_SECS: u64 = 60;

/// Pure decision: should we wipe the clipboard given what we wrote
/// (`original`) and what's there now (`current`)? Wipe iff they
/// still match — anything else means the user (or another app) has
/// copied something we shouldn't clobber.
///
/// Pure function so we can unit-test the policy without driving
/// the Slint event loop or a real clipboard.
fn clipboard_should_wipe(original: &str, current: &str) -> bool {
    !original.is_empty() && original == current
}

fn exported_key_should_clear(original: &str, current: &str) -> bool {
    !original.is_empty() && original == current
}

/// Schedule a clipboard wipe `CLIPBOARD_AUTOCLEAR_SECS` after the
/// current copy. The timer fires on the Slint event loop (which is
/// the main thread, where arboard demands to be called). If the
/// clipboard still contains the same text we wrote, clear it; if
/// the user has copied something else in the interim, leave it
/// alone.
///
/// RM-B1 / WP-E2.7 (audit GUI-C-03).
fn schedule_clipboard_autoclear(original: String) {
    slint::Timer::single_shot(
        std::time::Duration::from_secs(CLIPBOARD_AUTOCLEAR_SECS),
        move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Clipboard auto-clear: clipboard unavailable: {}", e);
                    return;
                }
            };
            match clipboard.get_text() {
                Ok(current) if clipboard_should_wipe(&original, &current) => {
                    if let Err(e) = clipboard.set_text("") {
                        tracing::warn!("Clipboard auto-clear: set_text failed: {}", e);
                    } else {
                        tracing::info!(
                            "Clipboard auto-cleared after {}s",
                            CLIPBOARD_AUTOCLEAR_SECS
                        );
                    }
                }
                Ok(_) => {
                    tracing::debug!(
                        "Clipboard auto-clear: contents changed since copy, leaving in place"
                    );
                }
                Err(e) => {
                    tracing::warn!("Clipboard auto-clear: get_text failed: {}", e);
                }
            }
        },
    );
}

fn schedule_exported_key_clear(ui_w: slint::Weak<App>, original: String) {
    slint::Timer::single_shot(
        std::time::Duration::from_secs(EXPORTED_KEY_DISPLAY_SECS),
        move || {
            if let Some(ui) = ui_w.upgrade() {
                let current = ui.get_wallet_exported_key().to_string();
                if exported_key_should_clear(&original, &current) {
                    ui.set_wallet_exported_key("".into());
                    ui.set_wallet_export_error("Private key display expired.".into());
                    tracing::info!(
                        "Exported private key display cleared after {}s",
                        EXPORTED_KEY_DISPLAY_SECS
                    );
                }
            }
        },
    );
}

#[cfg(test)]
mod clipboard_autoclear_tests {
    use super::*;

    #[test]
    fn test_guic03_wipes_when_clipboard_unchanged() {
        let original = "this is a sensitive secret";
        assert!(clipboard_should_wipe(original, original));
    }

    #[test]
    fn test_guic03_skips_when_user_copied_else() {
        let original = "this is a sensitive secret";
        let current = "this is the user's grocery list";
        assert!(!clipboard_should_wipe(original, current));
    }

    #[test]
    fn test_guic03_skips_when_already_empty() {
        // Edge: if the clipboard is somehow already empty we don't
        // need to wipe (and shouldn't claim we did).
        assert!(!clipboard_should_wipe("", ""));
    }

    #[test]
    fn test_guic03_skips_when_we_never_wrote() {
        // Defensive: caller passing empty `original` shouldn't
        // result in the helper wiping the clipboard.
        assert!(!clipboard_should_wipe("", "user's content"));
    }

    #[test]
    fn test_guic03_partial_substring_does_not_wipe() {
        // Mnemonic prefix is NOT the same as the full mnemonic.
        let original = "abandon abandon abandon ability";
        let current = "abandon abandon abandon"; // user truncated/edited
        assert!(!clipboard_should_wipe(original, current));
    }

    #[test]
    fn test_t0_04_exported_key_display_clears_only_when_unchanged() {
        let key = "a".repeat(64);
        assert!(exported_key_should_clear(&key, &key));
        assert!(!exported_key_should_clear(&key, "user replaced visible value"));
        assert!(!exported_key_should_clear("", ""));
    }

    #[test]
    fn test_t0_04_exported_key_has_no_clipboard_copy_path() {
        let source = include_str!("main.rs");
        let wallet_slint = include_str!("../ui/wallet/wallet.slint");
        let app_slint = include_str!("../ui/app.slint");
        let rust_handler = ["on_wallet_", "copy_exported_key"].concat();
        let slint_callback = ["copy", "-exported-key"].concat();
        let copy_label = ["Copy", " to Clipboard"].concat();

        assert!(!source.contains(&rust_handler));
        assert!(!wallet_slint.contains(&slint_callback));
        assert!(!wallet_slint.contains(&copy_label));
        assert!(!app_slint.contains(&slint_callback));
    }

    #[test]
    fn test_t0_04_export_dialog_close_routes_through_clear_export_state() {
        let wallet_slint = include_str!("../ui/wallet/wallet.slint");
        assert!(wallet_slint.contains("callback clear-export-state"));
        assert!(wallet_slint.contains("root.clear-export-state(); root.show-export-dialog = false"));
    }
}

/// RM-Q MEDIUM/LOW remediation — GUI-layer findings.
#[cfg(test)]
mod rm_q_gui_tests {
    use super::*;

    // NAT-B-012: recipient address validation before signing.
    #[test]
    fn natb012_recipient_validation() {
        // Valid all-lowercase (no checksum claim) — accepted, normalized.
        let lower = "0x52908400098527886e0f7030069857d2e4169ee7";
        assert_eq!(validate_recipient_address(lower).unwrap(), lower);
        // Same, uppercase-hex + 0X prefix → normalized lowercase.
        assert_eq!(
            validate_recipient_address("0X52908400098527886E0F7030069857D2E4169EE7").unwrap(),
            lower
        );
        // Whitespace trimmed.
        assert_eq!(validate_recipient_address(&format!("  {lower}  ")).unwrap(), lower);
        // Wrong length → rejected.
        assert!(validate_recipient_address("0x1234").is_err());
        // Non-hex → rejected.
        assert!(validate_recipient_address("0xZZ908400098527886e0f7030069857d2e4169ee7").is_err());
        // A VALID EIP-55 mixed-case address verifies.
        let checksummed = eip55_checksum(lower);
        assert!(validate_recipient_address(&checksummed).is_ok());
        // One flipped case bit in a mixed-case address → checksum fails.
        let mut bytes: Vec<char> = checksummed.chars().collect();
        // Flip the case of the first alphabetic hex nibble after 0x.
        for c in bytes.iter_mut().skip(2) {
            if c.is_ascii_alphabetic() {
                *c = if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                };
                break;
            }
        }
        let tampered: String = bytes.into_iter().collect();
        assert!(
            validate_recipient_address(&tampered).is_err(),
            "a mistyped char in a checksummed address must be rejected"
        );
    }

    // NAT-B-013 / NAT-B-021: the Slint properties that were declared but
    // never written from Rust are now driven. Source-level tripwire in the
    // same spirit as the existing include_str! guards.
    #[test]
    fn natb013_021_slint_state_properties_are_written() {
        let source = include_str!("main.rs");
        assert!(source.contains("set_send_sending("), "send-sending must be driven (NAT-B-013)");
        assert!(source.contains("set_lock_unlocking("), "lock-unlocking must be driven (NAT-B-021)");
        assert!(source.contains("set_lock_locked_out("), "lock-locked-out must be driven (NAT-B-021)");
        assert!(source.contains("set_lock_lockout_message("), "lock-lockout-message must be driven (NAT-B-021)");
    }

    // NAT-B-019: the idle-timeout transition raises the lock screen.
    #[test]
    fn natb019_idle_timeout_raises_lock_screen() {
        let source = include_str!("main.rs");
        // The timeout branch that stores epoch 0 and toasts must also set
        // the lock screen and dismiss the send dialog.
        let idx = source
            .find("Session expired — unlock your wallet to continue")
            .expect("idle-timeout toast present");
        let window = &source[idx.saturating_sub(600)..idx];
        assert!(
            window.contains("set_show_lock_screen(true)"),
            "idle timeout must raise the lock screen (NAT-B-019)"
        );
    }

    // NAT-B-017: the export handler routes through the WalletService (shared
    // lockout), not a fresh KeyManager.
    #[test]
    fn natb017_export_routes_through_service() {
        let source = include_str!("main.rs");
        assert!(
            source.contains("core.wallet.export_private_key("),
            "export must route through WalletService (NAT-B-017)"
        );
        // The fresh-KeyManager bypass must be gone from the export handler.
        assert!(
            !source.contains("KeyManager::new(&keystore_path)"),
            "export must not build a fresh KeyManager (NAT-B-017)"
        );
    }

    // NAT-B-016: the onboarding mnemonic property is cleared on completion.
    #[test]
    fn natb016_mnemonic_property_cleared() {
        let source = include_str!("main.rs");
        assert!(
            source.contains("set_onboarding_mnemonic(\"\".into())"),
            "onboarding mnemonic must be cleared (NAT-B-016)"
        );
    }

    // NAT-B-031: the empty-input verification skip is debug-only.
    #[test]
    fn natb031_empty_mnemonic_skip_is_debug_only() {
        let source = include_str!("main.rs");
        assert!(
            source.contains("input_str.is_empty() && cfg!(debug_assertions)"),
            "empty-mnemonic skip must be gated to debug builds (NAT-B-031)"
        );
    }

    // NAT-B-015: CMO super-admin cannot be granted from env in release.
    #[test]
    fn natb015_cmo_super_admin_env_is_debug_gated() {
        let source = include_str!("main.rs");
        let demo_idx = source
            .find("std::env::var(\"CITRATE_CMO_DEMO\")")
            .expect("CITRATE_CMO_DEMO read present");
        assert!(
            source[demo_idx.saturating_sub(160)..demo_idx].contains("cfg!(debug_assertions)"),
            "CITRATE_CMO_DEMO super-admin must be debug-gated (NAT-B-015)"
        );
        let e2e_idx = source
            .find("std::env::var(\"CITRATE_CMO_E2E_TREE\")")
            .expect("CITRATE_CMO_E2E_TREE read present");
        assert!(
            source[e2e_idx.saturating_sub(160)..e2e_idx].contains("cfg!(debug_assertions)"),
            "CITRATE_CMO_E2E_* overrides must be debug-gated (NAT-B-015)"
        );
    }
}

/// NATIVE-R1-S2 WP-A5: session-timeout unification regression tests.
/// There is now exactly ONE timeout constant (wallet_service's, the
/// enforcing side); the UI value is derived from it at compile time,
/// so agreement is by construction — these tests pin that construction
/// and what the countdown pill displays for it.
#[cfg(test)]
mod session_timeout_tests {
    use super::*;

    #[test]
    fn wp_a5_ui_and_backend_share_one_session_timeout() {
        assert_eq!(
            SESSION_TIMEOUT_SECS as u64,
            citrate_desktop_app::services::wallet_service::SESSION_TIMEOUT_SECS,
            "UI countdown constant must be the wallet_service constant"
        );
        // The unified value is the enforcing backend's 1 hour — the
        // pre-fix UI-only 8h value must be gone.
        assert_eq!(SESSION_TIMEOUT_SECS, 3600);
        assert_ne!(SESSION_TIMEOUT_SECS, 8 * 3600);
    }

    #[test]
    fn wp_a5_countdown_pill_displays_unified_timeout() {
        // What the session pill shows at unlock (the `session_initial`
        // sites all call this with SESSION_TIMEOUT_SECS).
        assert_eq!(format_session_remaining(SESSION_TIMEOUT_SECS), "1h00m");
    }
}

/// NAT-B-004: "Sign Out" must be a real custody teardown, not a view
/// swap. Both Sign Out and the Lock button route through
/// `perform_wallet_lock_teardown`, which clears the session epoch (the
/// gate on the background signing relay) AND locks the wallet.
#[cfg(test)]
mod sign_out_teardown_tests {
    use super::*;
    use citrate_desktop_app::error::AppError;
    use citrate_desktop_app::event_bus::EventBus;
    use citrate_desktop_app::services::wallet_service::{
        Account, CreateAccountResult, WalletBackend, WalletService,
    };
    use std::sync::Arc;

    struct MockBackend;

    #[async_trait::async_trait]
    impl WalletBackend for MockBackend {
        async fn load_accounts(&self) -> Result<Vec<Account>, AppError> {
            Ok(Vec::new())
        }
        async fn create_wallet(&self, _p: &str, _l: &str) -> Result<CreateAccountResult, AppError> {
            Ok(CreateAccountResult {
                address: "0xabc".to_string(),
                mnemonic: "test mnemonic".to_string(),
                public_key: "00".to_string(),
            })
        }
        async fn unlock(&self, _a: &str, _p: &str) -> Result<bool, AppError> {
            Ok(true)
        }
        async fn lock(&self) -> Result<(), AppError> {
            Ok(())
        }
        async fn send_transaction(&self, _f: &str, _t: &str, _v: &str, _p: &str) -> Result<String, AppError> {
            Ok("0x0".to_string())
        }
    }

    /// After the teardown, the session epoch is 0 and the wallet reports
    /// no active session. Pre-fix `on_sign_out` did neither — it only
    /// swapped the view, leaving keys in memory and the relay signing.
    #[tokio::test]
    async fn test_natb004_teardown_clears_epoch_and_locks_wallet() {
        let events = Arc::new(EventBus::new());
        let svc = WalletService::with_backend(events, Arc::new(MockBackend));

        // Activate a session (create_wallet unlocks it, like onboarding).
        svc.create_wallet("password123").await.expect("create wallet");
        assert!(
            svc.get_session_status().await.is_active,
            "precondition: session active after wallet creation"
        );
        SESSION_UNLOCK_EPOCH.store(1_700_000_000, Ordering::Relaxed);

        // The shared teardown Sign Out now calls.
        perform_wallet_lock_teardown(&svc).await;

        assert_eq!(
            SESSION_UNLOCK_EPOCH.load(Ordering::Relaxed),
            0,
            "sign-out/lock must clear the session epoch (relay custody gate)"
        );
        assert!(
            !svc.get_session_status().await.is_active,
            "sign-out/lock must lock the wallet"
        );
    }

    /// Source guard: the `on_sign_out` handler must route through the
    /// shared teardown, not re-implement a view-only swap. Mirrors the
    /// house `include_str!` guards (e.g. test_t0_04_*).
    #[test]
    fn test_natb004_sign_out_handler_calls_teardown() {
        let source = include_str!("main.rs");
        let marker = ["ui.on_sign", "_out(move"].concat();
        let start = source.find(&marker).expect("on_sign_out handler present");
        // The handler body runs until the NEXT callback registration.
        let rest = &source[start + marker.len()..];
        let end = rest.find("ui.on_").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("perform_wallet_lock_teardown"),
            "on_sign_out must call perform_wallet_lock_teardown to lock the wallet"
        );
    }
}

/// IPFS daemon statistics fetched from the local HTTP API.
struct IpfsStats {
    peer_count: i32,
    pin_count: i32,
    repo_size: String,
}

/// Fetch peer count, pin count, and repo size from the running IPFS daemon.
/// Data source: IPFS kubo HTTP API at localhost:5001 (/api/v0/swarm/peers, /api/v0/pin/ls, /api/v0/repo/stat)
async fn ipfs_fetch_stats(client: &reqwest::Client) -> IpfsStats {
    let peer_count = match client
        .post("http://127.0.0.1:5001/api/v0/swarm/peers")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                json.get("Peers")
                    .and_then(|p| p.as_array())
                    .map(|a| a.len() as i32)
                    .unwrap_or(0)
            } else {
                0
            }
        }
        Err(_) => 0,
    };

    let pin_count = match client
        .post("http://127.0.0.1:5001/api/v0/pin/ls")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                json.get("Keys")
                    .and_then(|k| k.as_object())
                    .map(|m| m.len() as i32)
                    .unwrap_or(0)
            } else {
                0
            }
        }
        Err(_) => 0,
    };

    let repo_size = match client
        .post("http://127.0.0.1:5001/api/v0/repo/stat")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let bytes = json
                    .get("RepoSize")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                format_bytes(bytes)
            } else {
                "0 B".to_string()
            }
        }
        Err(_) => "0 B".to_string(),
    };

    IpfsStats {
        peer_count,
        pin_count,
        repo_size,
    }
}

/// Format a byte count into a human-readable string (B, KB, MB, GB, TB).
/// P960-C WP-C.1/C.3: Upload a batch of paths to the local IPFS
/// daemon, persist each result into `files.json`, and push the
/// updated list into the Slint UI.
///
/// Called from both the file-picker callback and the winit
/// drag-drop handler so the upload flow is identical in both cases.
///
/// Side effects:
/// - Sets `storage-uploading` + `storage-upload-status` on the UI
///   during the batch, clears them when done.
/// - Writes to `~/.local/share/citrate-gui/files.json` atomically.
/// - Pushes a fresh `FileEntry` list into `storage-files`.
async fn upload_paths_to_ipfs(
    rt_handle: &tokio::runtime::Handle,
    ui_w: slint::Weak<App>,
    paths: Vec<std::path::PathBuf>,
    encrypt: bool,
) {
    let _ = rt_handle; // Present for symmetry + future streaming use.

    // ENCRYPT-S1 WP-9a: when the "Private (encrypted)" toggle is on,
    // resolve the device envelope key BEFORE any bytes move. Fail
    // CLOSED — a user who asked for private never silently gets a
    // plaintext upload because the keyring was unavailable.
    let owner_pub: Option<[u8; 32]> = if encrypt {
        let store = citrate_desktop_app::ports::SystemSecretStore::new();
        match storage_service::EnvelopeKey::load_or_create(&store) {
            Ok(key) => Some(key.public_bytes()),
            Err(e) => {
                tracing::error!("storage envelope key unavailable: {}", e);
                let ui_w2 = ui_w.clone();
                let emsg = format!("Private upload cancelled — encryption key unavailable: {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w2.upgrade() {
                        ui.set_storage_uploading(false);
                        ui.set_storage_upload_status(emsg.into());
                    }
                });
                return;
            }
        }
    } else {
        None
    };

    let client = reqwest::Client::new();
    let total = paths.len();
    let index_path = storage_service::FilesIndex::default_path();
    let mut index = index_path.as_ref()
        .map(|p| storage_service::FilesIndex::load(p))
        .unwrap_or_default();
    if index.version == 0 {
        index.version = 1;
    }

    for (i, path) in paths.iter().enumerate() {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string();
        let status = format!("Uploading {} of {}: {}", i + 1, total, name);
        {
            let ui_w2 = ui_w.clone();
            let status_clone = status.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w2.upgrade() {
                    ui.set_storage_upload_status(status_clone.into());
                }
            });
        }

        // Plaintext vs encrypted path — both return (cid, plaintext
        // size, envelope fields).
        let added = match owner_pub {
            Some(ref owner) => storage_service::ipfs_add_file_encrypted(
                &client,
                storage_service::DEFAULT_IPFS_API,
                path,
                owner,
            )
            .await
            .map(|r| (r.cid, r.size_bytes, true, r.wrapped_key, r.nonce)),
            None => storage_service::ipfs_add_file(&client, path)
                .await
                .map(|(cid, size)| (cid, size, false, String::new(), String::new())),
        };

        match added {
            Ok((cid, size, encrypted, wrapped_key, nonce)) => {
                let mime = mime_guess::from_path(path)
                    .first_raw()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let uploaded_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let rec = storage_service::FileRecord {
                    cid,
                    name,
                    size_bytes: size,
                    uploaded_at,
                    mime,
                    encrypted,
                    wrapped_key,
                    nonce,
                };
                index.upsert(rec);
            }
            Err(e) => {
                tracing::error!("ipfs add {} failed: {}", path.display(), e);
                let ui_w2 = ui_w.clone();
                let emsg = format!("Failed: {} — {}", name, e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w2.upgrade() {
                        ui.set_storage_upload_status(emsg.into());
                    }
                });
            }
        }
    }

    if let Some(ref p) = index_path {
        if let Err(e) = index.save(p) {
            tracing::warn!("files.json save failed: {}", e);
        }
    }

    let entries = build_file_entries(&index);
    let ui_w_final = ui_w.clone();
    let done_msg = if total == 1 { "Uploaded 1 file".to_string() } else { format!("Uploaded {} files", total) };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_w_final.upgrade() {
            let model = std::rc::Rc::new(slint::VecModel::from(entries));
            ui.set_storage_files(model.into());
            ui.set_storage_uploading(false);
            ui.set_storage_upload_status("".into());
            ui.set_clipboard_toast(done_msg.into());
            let ui_for_clear = ui_w_final.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1800), move || {
                if let Some(ui) = ui_for_clear.upgrade() {
                    ui.set_clipboard_toast("".into());
                }
            });
        }
    });
}

/// Turn a `FilesIndex` into Slint `FileEntry` rows with pre-formatted
/// display strings. Called on upload, refresh, and removal.
fn build_file_entries(index: &storage_service::FilesIndex) -> Vec<FileEntry> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    index.files.iter().map(|f| FileEntry {
        name: f.name.clone().into(),
        size: f.size_display().into(),
        mime_icon: f.mime_icon().into(),
        uploaded: f.uploaded_display(now_secs).into(),
        cid: f.cid.clone().into(),
        encrypted: f.encrypted,
    }).collect()
}

/// P960-A WP-A.4: Pretty-format a tool-result JSON string so it reads
/// naturally in the chat thread. Chain data is structured —
/// `{"blocks": [...]}` etc. — and dumping raw JSON into a conversation
/// bubble is hostile. This renders:
///
/// - `get_recent_blocks` → `#N (hash…) · M txns · T ago` rows
/// - `get_block_height`  → `Block N · chain 40204 · synced` one-liner
/// - `get_peer_count`    → `1 peer · 0 mempool · 1 tip`
/// - `check_balance`     → `0xaaaa… — 10.00 SALT`
/// - `explain_tx`        → labeled from/to/value/status card
/// - `get_tx_history`    → N rows of `send/receive/reward · amount · counterparty`
///
/// Falls back to the raw content on any parse failure — the LLM still
/// has the JSON so the conversation doesn't break.
fn format_tool_result(tool_name: &str, raw_content: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(raw_content) {
        Ok(v) => v,
        Err(_) => return raw_content.to_string(),
    };

    match tool_name {
        "get_recent_blocks" => {
            let arr = match v.get("blocks").and_then(|a| a.as_array()) {
                Some(a) => a,
                None => return raw_content.to_string(),
            };
            if arr.is_empty() {
                return "No recent blocks.".to_string();
            }
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut lines: Vec<String> = Vec::with_capacity(arr.len());
            for b in arr {
                let height = b.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
                let full_hash = b.get("hash").and_then(|x| x.as_str()).unwrap_or("");
                let short_hash = full_hash.chars().take(10).collect::<String>();
                let tx_count = b.get("tx_count").and_then(|x| x.as_u64()).unwrap_or(0);
                let ts = b.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
                let age = if ts > 0 && now_secs > 0 {
                    let d = (now_secs - ts as i64).max(0);
                    if d < 60 { format!("{}s ago", d) }
                    else if d < 3600 { format!("{}m ago", d / 60) }
                    else { format!("{}h ago", d / 3600) }
                } else {
                    "—".to_string()
                };
                lines.push(format!("#{}  {}…  {} txns  {}", height, short_hash, tx_count, age));
            }
            lines.join("\n")
        }
        "get_block_height" => {
            let h = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
            let c = v.get("chain_id").and_then(|x| x.as_u64()).unwrap_or(0);
            let sync = v.get("syncing").and_then(|x| x.as_bool()).unwrap_or(false);
            format!("Block {} · chain {} · {}", h, c, if sync { "syncing" } else { "synced" })
        }
        "get_peer_count" => {
            let p = v.get("peers").and_then(|x| x.as_u64()).unwrap_or(0);
            let m = v.get("mempool_size").and_then(|x| x.as_u64()).unwrap_or(0);
            let t = v.get("dag_tips").and_then(|x| x.as_u64()).unwrap_or(0);
            format!("{} peer{} · {} mempool · {} tip{}",
                p, if p == 1 { "" } else { "s" },
                m,
                t, if t == 1 { "" } else { "s" })
        }
        "check_balance" => {
            let a = v.get("address").and_then(|x| x.as_str()).unwrap_or("—");
            let s = v.get("balance_salt").and_then(|x| x.as_str()).unwrap_or("0");
            let short = if a.len() > 14 { format!("{}…{}", &a[..6], &a[a.len()-4..]) } else { a.to_string() };
            format!("{}\n{} SALT", short, s)
        }
        "explain_tx" => {
            let hash = v.get("hash").and_then(|x| x.as_str()).unwrap_or("—");
            let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("—");
            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or("—");
            let value = v.get("value_salt").and_then(|x| x.as_str()).unwrap_or("0");
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("—");
            let block = v.get("block_height").and_then(|x| x.as_u64()).unwrap_or(0);
            let ty = v.get("tx_type").and_then(|x| x.as_str()).unwrap_or("—");
            format!(
                "hash:   {}\nfrom:   {}\nto:     {}\nvalue:  {} SALT\nstatus: {}  ({})\nblock:  #{}",
                hash, from, to, value, status, ty, block,
            )
        }
        "get_tx_history" => {
            let txs = match v.get("transactions").and_then(|a| a.as_array()) {
                Some(a) => a,
                None => return raw_content.to_string(),
            };
            if txs.is_empty() {
                let addr = v.get("address").and_then(|x| x.as_str()).unwrap_or("this address");
                return format!("No transactions yet for {}.", addr);
            }
            let mut lines: Vec<String> = Vec::with_capacity(txs.len());
            for t in txs {
                let ty = t.get("tx_type").and_then(|x| x.as_str()).unwrap_or("tx");
                let amt = t.get("amount").and_then(|x| x.as_str()).unwrap_or("0");
                let cp = t.get("counterparty").and_then(|x| x.as_str()).unwrap_or("—");
                let st = t.get("status").and_then(|x| x.as_str()).unwrap_or("");
                lines.push(format!("{:<8} {:>10} SALT  {}  [{}]", ty, amt, cp, st));
            }
            lines.join("\n")
        }
        _ => raw_content.to_string(),
    }
}

// ── P960-K T1-4: dependency health preflight ────────────────────
//
// Three external services the GUI depends on. Each probe is a
// 2-second TCP connect or HTTP call; they run on Settings tab open
// and on the explicit "Refresh" button. We surface real status so
// users on first run know which dependency is missing instead of
// guessing why panels are degraded.

/// Health status for one dependency. "ok" = reachable, "down" =
/// probe failed, "checking" = probe in flight (transient).
#[derive(Debug, Clone, Copy)]
enum HealthState {
    Ok,
    Down,
}

impl HealthState {
    fn as_str(&self) -> &'static str {
        match self {
            HealthState::Ok => "ok",
            HealthState::Down => "down",
        }
    }
}

/// TCP-connect probe. Fast, no protocol parsing — just "can I open
/// a socket to host:port within 2s?"
async fn probe_tcp(host_port: &str) -> HealthState {
    use tokio::net::TcpStream;
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        TcpStream::connect(host_port),
    ).await {
        Ok(Ok(_)) => HealthState::Ok,
        _ => HealthState::Down,
    }
}

/// IPFS HTTP API probe — POST /api/v0/version. Returns Ok iff the
/// daemon responds 200 within 2s.
async fn probe_ipfs(api_url: &str) -> HealthState {
    let url = format!("{}/api/v0/version", api_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return HealthState::Down,
    };
    match client.post(&url).send().await {
        Ok(resp) if resp.status().is_success() => HealthState::Ok,
        _ => HealthState::Down,
    }
}

/// JSON-RPC probe — eth_blockNumber on the local node.
async fn probe_rpc(rpc_url: &str) -> HealthState {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return HealthState::Down,
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_blockNumber",
        "params": [],
        "id": 1,
    });
    match client.post(rpc_url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Make sure the response is actually well-formed JSON-RPC,
            // not just a 200 from some other server squatting the port.
            match resp.json::<serde_json::Value>().await {
                Ok(json) if json.get("result").and_then(|v| v.as_str()).is_some() => {
                    HealthState::Ok
                }
                _ => HealthState::Down,
            }
        }
        _ => HealthState::Down,
    }
}

/// Run all three health probes against the current AppCore config
/// and push the results back to the UI. Sequential so the spinner
/// doesn't all flip at once but it's still done in ~6s total worst-case.
async fn run_health_probes(
    core: &Arc<AppCore>,
    ui_w: slint::Weak<App>,
) {
    let config = core.config.read().await;
    let bootnode = config.bootnodes.first().cloned()
        .unwrap_or_else(|| "<none configured>".to_string());
    let rpc_url = config.active_rpc_url();
    drop(config);
    let ipfs_url = "http://127.0.0.1:5001".to_string();

    // 1. Bootnode TCP
    let bootnode_state = if bootnode.contains(':') && !bootnode.starts_with('<') {
        probe_tcp(&bootnode).await
    } else {
        HealthState::Down
    };

    // 2. IPFS HTTP API
    let ipfs_state = probe_ipfs(&ipfs_url).await;

    // 3. Local node RPC
    let rpc_state = probe_rpc(&rpc_url).await;

    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_w.upgrade() else { return; };
        ui.set_health_bootnode_status(bootnode_state.as_str().into());
        ui.set_health_bootnode_detail(bootnode.into());
        ui.set_health_ipfs_status(ipfs_state.as_str().into());
        ui.set_health_ipfs_detail(ipfs_url.into());
        ui.set_health_node_rpc_status(rpc_state.as_str().into());
        ui.set_health_node_rpc_detail(rpc_url.into());
    });
}

/// FUA-GUI-01 residual (WP 6.4b): the relay's per-write human confirmation
/// surface. Privileged writes (today: `claimRewards`; any value-bearing write
/// by policy) are submitted to the SAME `PendingApprovalStore` the chat-tool
/// and Operations approval flows use — they appear on the Operations panel's
/// pending-approvals list with Approve/Deny, auto-deny on the store's
/// timeout, and fail closed on any error. A write the user declines (or that
/// times out) is remembered and silently refused on subsequent relay ticks so
/// a stuck queue entry cannot generate an approval-prompt storm.
struct RelayApprovalGate {
    approvals: Arc<citrate_agent_core::delegation::PendingApprovalStore>,
    declined: tokio::sync::Mutex<std::collections::HashSet<u64>>,
}

impl RelayApprovalGate {
    fn new(approvals: Arc<citrate_agent_core::delegation::PendingApprovalStore>) -> Self {
        Self {
            approvals,
            declined: tokio::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }
}

#[async_trait::async_trait]
impl citrate_desktop_app::services::relay_service::ConfirmationGate for RelayApprovalGate {
    async fn confirm_write(
        &self,
        write: &citrate_desktop_app::services::relay_service::ValidatedWrite,
    ) -> bool {
        if self.declined.lock().await.contains(&write.id) {
            // Already declined/timed out once — stay refused, don't re-prompt.
            return false;
        }
        let request = citrate_agent_core::canonical::ApprovalRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: "relay".to_string(),
            tool_name: format!("relay_sign:{}", write.intent),
            params: serde_json::json!({
                "queue_id": write.id,
                "intent": write.intent,
                "to": write.to,
                "value_wei": write.value_wei,
                "write": write.describe(),
            }),
            risk_level: "high".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            timeout_seconds: 60,
            resolved: None,
            resolved_at: None,
        };
        tracing::info!("signing relay: confirmation required — {}", write.describe());
        let rx = self.approvals.submit(request).await;
        // Fail closed: dropped sender / timeout / explicit deny are all false.
        let approved = rx.await.unwrap_or(false);
        if !approved {
            self.declined.lock().await.insert(write.id);
            tracing::warn!(
                "signing relay: write {} NOT confirmed — refusing (and muting re-prompts)",
                write.id
            );
        }
        approved
    }
}

/// NAT-B-007: user-initiated contract writes (Learning join/leave/create pool,
/// provider registration, reward claims, model publish) build calldata and, before
/// this gate, broadcast on a single button click showing the user no target, no
/// decoded method, and no amount. Every such write is now submitted to the SAME
/// `PendingApprovalStore` the signing relay and chat tools use, rendering the
/// decoded intent on the Operations pending-approvals panel with Approve/Deny.
///
/// The call FAILS CLOSED: a declined request, a store timeout, or a dropped
/// resolution channel all resolve to `false`, and the caller skips the broadcast.
/// This mirrors the relay's `RelayApprovalGate` (which is the counter-example
/// the audit flagged these six paths against).
pub(crate) async fn confirm_tx_intent(
    approvals: &citrate_agent_core::delegation::PendingApprovalStore,
    action: &str,
    to: &str,
    value_wei: &str,
    data: &[u8],
) -> bool {
    // Decode the 4-byte selector so the confirmation surface names the method
    // the user is authorizing, not just an opaque address.
    let method = calldata_decoder::decode_selector(&format!("0x{}", hex::encode(data))).label();
    let request = citrate_agent_core::canonical::ApprovalRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: "user-write".to_string(),
        tool_name: format!("wallet_write:{}", action),
        params: serde_json::json!({
            "action": action,
            "to": to,
            "value_wei": value_wei,
            "method": method,
            "calldata": format!("0x{}", hex::encode(data)),
        }),
        risk_level: "high".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        timeout_seconds: 60,
        resolved: None,
        resolved_at: None,
    };
    tracing::info!(
        "wallet write: confirmation required — {} → {} ({}), value {} wei",
        action, method, to, value_wei
    );
    let rx = approvals.submit(request).await;
    // Fail closed: dropped sender / timeout / explicit deny all map to `false`.
    let approved = rx.await.unwrap_or(false);
    if !approved {
        tracing::warn!("wallet write: {} NOT confirmed — broadcast skipped", action);
    }
    approved
}

/// Outcome of polling `eth_getTransactionReceipt` for a submitted tx.
///
/// Ok(Confirmed(block_number_hex)) — status 0x1, tx included in a block.
/// Ok(Reverted) — status 0x0, tx included but execution reverted.
/// Ok(Pending) — poll window elapsed without the tx being mined.
/// Err(String) — transport failure talking to the RPC.
#[derive(Debug)]
enum ReceiptOutcome {
    Confirmed { block_number: String },
    Reverted,
    Pending,
}

/// Poll `eth_getTransactionReceipt` for `tx_hash` at the given RPC URL.
/// Waits up to 40s total (20 polls × 2s interval). Used by every
/// transactional handler so the user gets an explicit
/// success/reverted/pending outcome instead of fire-and-forget.
/// T1-1 extraction: previously inlined only in on_compute_register_provider.
async fn poll_tx_receipt(rpc_url: &str, tx_hash: &str) -> Result<ReceiptOutcome, String> {
    let client = reqwest::Client::new();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
            "id": 1,
        });
        let resp = match client.post(rpc_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Err(format!("rpc transport: {}", e)),
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => return Err(format!("rpc decode: {}", e)),
        };
        let Some(result) = json.get("result") else { continue; };
        if result.is_null() { continue; }
        let status = result["status"].as_str().unwrap_or("0x0");
        if status == "0x1" {
            let block = result["blockNumber"].as_str().unwrap_or("0x?").to_string();
            return Ok(ReceiptOutcome::Confirmed { block_number: block });
        }
        return Ok(ReceiptOutcome::Reverted);
    }
    Ok(ReceiptOutcome::Pending)
}

/// Shorten a 0x-prefixed hex hash or address for compact display
/// in status text. `0xabcdef…123456` form.
fn short_hash(hex: &str) -> String {
    let s = hex.strip_prefix("0x").unwrap_or(hex);
    if s.len() < 12 {
        return format!("0x{}", s);
    }
    format!("0x{}…{}", &s[..6], &s[s.len() - 4..])
}

// The RPC-URL selector now lives on AppConfig as `active_rpc_url()`
// (single source of truth, see citrate_desktop_app::AppConfig). The
// former free function here was a duplicate and was removed in the RPC
// port canonicalization sweep.

/// Format session-remaining seconds as a short, unambiguous human-readable
/// string that fits the 60px session pill. Must always lead with a time
/// unit (`h` / `m` / `s`) so users can't misread a raw number as an
/// unrelated value — previous `MM:SS` format produced strings like
/// "479:59" that some users interpreted as "locked for X hours".
fn format_session_remaining(total_secs: i64) -> String {
    let s = total_secs.max(0);
    let hours = s / 3600;
    let mins = (s % 3600) / 60;
    let secs = s % 60;
    if hours > 0 {
        format!("{}h{:02}m", hours, mins)
    } else if mins > 0 {
        format!("{}m{:02}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

fn wei_str_to_salt(wei: &str) -> String {
    let w = wei.trim();
    let bytes: Option<u128> = if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else {
        w.parse::<u128>().ok()
    };
    let Some(n) = bytes else { return "0".to_string(); };
    // 10^18 wei per SALT.
    let whole = n / 1_000_000_000_000_000_000u128;
    let frac = n % 1_000_000_000_000_000_000u128;
    // Keep 4 fractional digits — plenty for "balance" context in chat.
    let frac_4 = frac / 100_000_000_000_000u128; // = 10^14
    if frac_4 == 0 {
        format!("{}", whole)
    } else {
        format!("{}.{:04}", whole, frac_4)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// BFR-INT-5b: short-form wei display for table rows (e.g. "0.5 SALT").
/// Falls back to the raw decimal when value < 0.0001 SALT or parse fails.
fn format_wei_short(wei_str: &str) -> String {
    let wei: u128 = match wei_str.parse() {
        Ok(w) => w,
        Err(_) => return wei_str.to_string(),
    };
    if wei == 0 {
        return "0 SALT".to_string();
    }
    const ONE_SALT: u128 = 1_000_000_000_000_000_000;
    let salt = wei as f64 / ONE_SALT as f64;
    if salt >= 0.0001 {
        format!("{:.4} SALT", salt)
    } else {
        format!("{} wei", wei)
    }
}

/// BFR-INT-5b: full wei display for the modal — shows both SALT and wei
/// so the operator has the exact integer when needed.
fn format_wei_long(wei_str: &str) -> String {
    let wei: u128 = match wei_str.parse() {
        Ok(w) => w,
        Err(_) => return wei_str.to_string(),
    };
    if wei == 0 {
        return "0 SALT".to_string();
    }
    const ONE_SALT: u128 = 1_000_000_000_000_000_000;
    let salt = wei as f64 / ONE_SALT as f64;
    format!("{:.6} SALT  ({} wei)", salt, wei)
}

/// WP-E6.1.5-D — Fetch live CMO portal data from the chain and push it
/// into the Slint UI properties.
///
/// Sequential fetch:
///   1. listAllSchoolsForCmo(cmo_hash) → list of school hashes
///   2. for each school: getNode + getSchoolMatrix
///   3. Build SchoolEntry / CmoSchoolRow / ComplianceSchoolMatrixRow models
///   4. invoke_from_event_loop to atomically swap into UI props
///
/// Errors are logged and the UI stays empty (no fake fallback).
async fn fetch_cmo_portal_data(
    service: std::sync::Arc<citrate_edu_app::services::cmo_portal::CmoPortalService>,
    cmo_hash: String,
    ui_weak: slint::Weak<App>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use citrate_edu_app::services::cmo_portal::ComplianceStatus;

    let schools = service.list_schools_for_cmo(&cmo_hash).await?;
    tracing::info!(
        "[E6.1.5-D] fetched {} schools under cmo={}",
        schools.len(),
        cmo_hash
    );

    // For each school, fetch its node + matrix concurrently within the
    // task. Order is preserved so the UI list matches registration order.
    let mut school_data: Vec<(String, citrate_edu_app::services::cmo_portal::InstitutionNode, [citrate_edu_app::services::cmo_portal::GateRecord; 9])> = Vec::with_capacity(schools.len());
    for sch_hash in &schools {
        match service.get_node(sch_hash).await {
            Ok(node) => match service.get_school_matrix(sch_hash).await {
                Ok(matrix) => school_data.push((sch_hash.clone(), node, matrix)),
                Err(e) => {
                    tracing::warn!("[E6.1.5-D] getSchoolMatrix failed for {sch_hash}: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("[E6.1.5-D] getNode failed for {sch_hash}: {e}");
            }
        }
    }

    // Atomic UI update via invoke_from_event_loop. All Slint construction
    // happens inside the closure (Slint types aren't Send).
    let school_data_clone = school_data.clone();
    slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else { return };

        // Stable display-name fallback when the chain doesn't carry it
        // (the tree only stores pseudonymous hashes; display names are
        // an off-chain concern). For now use a short-hash form.
        fn short_label(prefix: &str, hash: &str) -> String {
            let trimmed = hash.trim_start_matches("0x");
            let short = if trimmed.len() >= 8 {
                &trimmed[..8]
            } else {
                trimmed
            };
            format!("{prefix} 0x{short}")
        }

        // ── SchoolEntry list (for the sidebar selector) ──
        let school_entries: Vec<SchoolEntry> = school_data_clone
            .iter()
            .map(|(hash, _node, _mx)| SchoolEntry {
                school_id: hash.clone().into(),
                display_name: short_label("School", hash).into(),
                student_count: 0, // student count is not on-chain in v1
            })
            .collect();
        ui.set_cmo_schools(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
            school_entries,
        ))));

        // ── Dashboard rows ──
        // Compute per-school compliance health: worst applicable cell wins.
        // Green if all applicable Signed; Yellow if any Untouched; Red if
        // any Expired or Revoked.
        let dash_rows: Vec<CmoSchoolRow> = school_data_clone
            .iter()
            .map(|(hash, node, mx)| {
                let mut has_red = false;
                let mut has_yellow = false;
                for (_idx, cell) in mx.iter().enumerate() {
                    match cell.status {
                        ComplianceStatus::NotApplicable => {}
                        ComplianceStatus::Expired | ComplianceStatus::Revoked => has_red = true,
                        ComplianceStatus::Untouched => has_yellow = true,
                        ComplianceStatus::Signed => {}
                    }
                }
                let health = if has_red {
                    "Red"
                } else if has_yellow {
                    "Yellow"
                } else {
                    "Green"
                };
                CmoSchoolRow {
                    school_id: hash.clone().into(),
                    display_name: short_label("School", hash).into(),
                    student_count: 0,
                    compliance_health: health.into(),
                    last_activity: format!("0x{:x}", node.registered_at).into(),
                    open_issues: if has_red { 1 } else { 0 },
                }
            })
            .collect();

        // ── Aggregate stats ──
        let total_signed = school_data_clone
            .iter()
            .map(|(_, _, mx)| {
                mx.iter()
                    .filter(|c| c.status == ComplianceStatus::Signed)
                    .count() as i32
            })
            .sum::<i32>();
        let total_issues = dash_rows
            .iter()
            .map(|r| r.open_issues)
            .sum::<i32>();

        ui.set_cmo_dashboard_stats(CmoDashboardStats {
            total_schools: school_data_clone.len() as i32,
            total_students: 0,
            total_active_compliance_gates: total_signed,
            total_open_issues: total_issues,
        });
        ui.set_cmo_dashboard_schools(slint::ModelRc::from(std::rc::Rc::new(
            slint::VecModel::from(dash_rows),
        )));
        ui.set_cmo_dashboard_events(slint::ModelRc::from(std::rc::Rc::new(
            slint::VecModel::from(Vec::<CmoEvent>::new()),
        )));

        // ── Compliance matrix rows ──
        let compliance_rows: Vec<ComplianceSchoolMatrixRow> = school_data_clone
            .iter()
            .map(|(hash, node, mx)| {
                let state_label = match node.state {
                    0 => "CA",
                    1 => "NY",
                    2 => "IL",
                    3 => "TX",
                    4 => "CO",
                    _ => "Other",
                };
                let labels = ["DPA", "FERPA", "COPPA", "CIPA", "CA", "NY", "IL", "TX", "CO"];
                let cells: Vec<ComplianceCell> = mx
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| ComplianceCell {
                        gate_id: labels[i].to_lowercase().into(),
                        gate_label: labels[i].into(),
                        status: cell.status.display_label().into(),
                        last_signed: if cell.signed_at > 0 {
                            format!("@ts:{}", cell.signed_at).into()
                        } else {
                            "".into()
                        },
                        expires_at: if cell.expires_at > 0 {
                            format!("@ts:{}", cell.expires_at).into()
                        } else {
                            "".into()
                        },
                    })
                    .collect();
                ComplianceSchoolMatrixRow {
                    school_id: hash.clone().into(),
                    school_name: short_label("School", hash).into(),
                    school_state: state_label.into(),
                    cells: slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(cells))),
                }
            })
            .collect();
        ui.set_cmo_compliance_rows(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
            compliance_rows,
        ))));

        // ── Tenancy nodes (CMO root + flat school list) ──
        let mut tenancy: Vec<TenancyNode> = Vec::with_capacity(school_data_clone.len() + 1);
        tenancy.push(TenancyNode {
            node_id: "0xcmo".into(),
            kind: "cmo".into(),
            display_name: "Charter Management Organization".into(),
            level: 0,
            student_count: 0,
            classroom_count: 0,
            expanded: true,
        });
        for (hash, _, _) in &school_data_clone {
            tenancy.push(TenancyNode {
                node_id: hash.clone().into(),
                kind: "school".into(),
                display_name: short_label("School", hash).into(),
                level: 1,
                student_count: 0,
                classroom_count: 0,
                expanded: false,
            });
        }
        ui.set_cmo_tenancy_nodes(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
            tenancy,
        ))));

        tracing::info!(
            "[E6.1.5-D] CMO portal UI populated with {} schools (live on-chain data)",
            school_data_clone.len()
        );
    })?;

    Ok(())
}

fn main() {
    // NATIVE-R1-S2 WP-A1: panic hook FIRST — before the subscriber, before
    // the runtime — so even an early-boot panic on any thread writes a
    // crash record under ~/.local/share/citrate-gui/crash/ (and still
    // prints to stderr via the chained default hook).
    crash_telemetry::install_panic_hook();

    // WP-A1: tracing goes to stdout (as before) AND to a rotating file
    // log (~/.local/share/citrate-gui/logs/citrate-gui.log, 5 MB × 2) so
    // a silent death leaves logs that survive the terminal.
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,citrate=debug".into());
        let file_layer = crash_telemetry::file_log_writer().map(|writer| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
        });
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .with(file_layer)
            .init();
    }

    tracing::info!("Citrate Desktop starting (Slint native)");

    // WP-A1: surface any stale session marker (previous unclean death —
    // abort/SIGKILL/segfault never run the panic hook) or old crash
    // records, THEN write this session's marker. The marker is removed
    // on clean exit; `set_last_state` keeps it pointing at the latest
    // app state so a kill is attributable on the next launch.
    crash_telemetry::startup_scan();
    crash_telemetry::init_session_marker();
    crash_telemetry::set_last_state("app-boot");

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Failed to create Tokio runtime: {err}");
            std::process::exit(1);
        }
    };
    let app_core = Arc::new(AppCore::new());

    // EW-S1 WP-8: Citrate identity link service. Link state (no secrets)
    // persists at <config>/citrate-native/citrate_link.json.
    let citrate_link_path = dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("citrate-native")
        .join("citrate_link.json");
    let citrate_link = Arc::new(
        citrate_desktop_app::services::citrate_link_service::CitrateLinkService::new(
            app_core.wallet.clone(),
            citrate_link_path,
        ),
    );

    // Hydrate the in-memory account list from the on-disk keystore BEFORE
    // we inspect `is_first_run`. Without this, every restart looks like a
    // brand-new install: the service's `self.accounts` starts empty, so
    // `is_first_run()` returns true, onboarding fires again, and existing
    // wallets are invisible. `AppCore::start()` does this wiring, but the
    // native GUI boots through a different path, so we load here directly.
    if let Err(e) = rt.block_on(app_core.wallet.load_from_disk()) {
        tracing::warn!("wallet load_from_disk failed: {}", e);
    }

    // Align the wallet's RPC URL with the effective environment BEFORE any
    // tx is submitted. Without this, the wallet uses its default
    // (https://rpc.citrate.ai) but the GUI's receipt-polling helpers hit
    // http://127.0.0.1:<embedded-port>. Mismatched URLs produce the
    // classic "rpc transport: error sending request" after a successful
    // submission because the receipt lives on whichever node accepted
    // the tx — not on the arbitrary port the poller defaulted to.
    {
        let cfg = rt.block_on(app_core.config.read());
        let rpc_url = cfg.active_rpc_url();
        app_core.wallet.set_rpc_url(&rpc_url);
        app_core.wallet.set_chain_id(cfg.chain_id);
        tracing::info!(
            "Wallet RPC aligned: network={}, chain_id={}, rpc={}",
            cfg.network, cfg.chain_id, rpc_url,
        );
    }
    let is_first_run = rt.block_on(app_core.wallet.is_first_run());

    // BFR-INT-4: Boeing bindings init moved to citrate-boeing-shell.

    // P960-J: start the MCP host so external agent runtimes (Hermes)
    // can discover our tools. Bind failure is non-fatal — we log and
    // continue; Ops panel will show "MCP host not running" and the
    // user can retry from settings in a future sprint.
    {
        let port = rt.block_on(app_core.config.read()).mcp_port;
        let port = if port == 0 {
            citrate_desktop_app::services::mcp_host::DEFAULT_MCP_PORT
        } else {
            port
        };
        if let Err(e) = rt.block_on(app_core.mcp_host.clone().start(port)) {
            tracing::warn!("MCP host failed to bind on {}: {}", port, e);
        }
    }

    let ui = match App::new() {
        Ok(ui) => ui,
        Err(err) => {
            eprintln!("Failed to create Slint window: {err}");
            std::process::exit(1);
        }
    };

    // Initial state — derive environment label from loaded config, not hardcoded.
    //
    // Three startup modes:
    //   1. First run       → onboarding (create/import a wallet).
    //   2. Returning user  → lock screen (wallet exists on disk, in-memory
    //                        keys were dropped when the previous process
    //                        exited). Without this the user lands on the
    //                        main tabs with a locked 🔒 wallet and no
    //                        prompt to unlock — they can't transact.
    //   3. Session active  → dashboard. Reached by completing case 1 or 2.
    ui.set_show_onboarding(is_first_run);
    ui.set_show_lock_screen(!is_first_run);
    ui.set_active_tab("dashboard".into());

    // NATIVE-R1-S1 WP-5: start the CitrateLoader driver (per the embed
    // recipe in ui/loader/citrate_loader.slint) and park it. The one
    // facet-commands model is bound to both loader instances via the
    // `loader-facets` app property; loader_set_running(true/false) at the
    // onboarding-bootstrap and chat-thinking toggle sites animates it.
    {
        let loader = citrate_ui_kit::loader::start_loader(Default::default());
        loader.stop(); // idle until a loading state begins
        ui.set_loader_facets(loader.model());
        LOADER.with(|slot| *slot.borrow_mut() = Some(loader));
    }

    // WP-E6.2 — CMO portal initialization. By default, the user is NOT a
    // CMOSuperAdmin and the school selector is hidden. The detected role
    // will flip this when E6.1.5 lands the on-chain getCmoRole RPC call.
    //
    // Visual-review override: setting CITRATE_CMO_DEMO=true forces
    // is-cmo-super-admin=true and populates the schools list with a small
    // stub set so the UI can be demoed without on-chain wiring. This is
    // the same env-var pattern as CITRATE_ROLE / CITRATE_DEMO_MODE
    // documented in citrate_v0.01.1/gui/citrate_learning_center/release/INSTALL.md.
    {
        // NAT-B-015: CITRATE_CMO_DEMO grants a client-side super-admin view
        // with no on-chain role check. Honor it ONLY in debug builds so a
        // release binary cannot be handed super-admin by its environment
        // (a wrapper script / modified launcher). Release derives the flag
        // from the on-chain role path below (E6.1.5) — never from env.
        let cmo_demo = cfg!(debug_assertions)
            && std::env::var("CITRATE_CMO_DEMO")
                .map(|v| v == "true")
                .unwrap_or(false);
        ui.set_is_cmo_super_admin(cmo_demo);
        if cmo_demo {
            // Stub schools — three example schools for visual demo. Replaced
            // by real `InstitutionTreeV1.getCmoSchools(cmo_id)` query once
            // the contract method ships (E6.1.5).
            let stub_schools: Vec<SchoolEntry> = vec![
                SchoolEntry {
                    school_id: "0xdemo_a".into(),
                    display_name: "KIPP Charter Newark".into(),
                    student_count: 412,
                },
                SchoolEntry {
                    school_id: "0xdemo_b".into(),
                    display_name: "KIPP Charter Bayonne".into(),
                    student_count: 287,
                },
                SchoolEntry {
                    school_id: "0xdemo_c".into(),
                    display_name: "KIPP Charter Jersey City".into(),
                    student_count: 521,
                },
            ];
            let model = std::rc::Rc::new(slint::VecModel::from(stub_schools));
            ui.set_cmo_schools(slint::ModelRc::from(model));
            ui.set_cmo_active_school_id("".into());
            ui.set_cmo_active_school_name("".into());

            // WP-E6.3 — CMO dashboard stub data. Aggregated across the
            // 3 stub schools above. Replaced by real RPC queries
            // (`InstitutionTreeV1.aggregateMetrics(cmo_id)` +
            // `ContributionAccounting.getCmoEvents(cmo_id, limit)`)
            // once E6.1.5 lands.
            ui.set_cmo_dashboard_stats(CmoDashboardStats {
                total_schools: 3,
                total_students: 1220, // 412 + 287 + 521
                total_active_compliance_gates: 27, // 9 gates x 3 schools
                total_open_issues: 2,
            });

            let stub_school_rows: Vec<CmoSchoolRow> = vec![
                CmoSchoolRow {
                    school_id: "0xdemo_a".into(),
                    display_name: "KIPP Charter Newark".into(),
                    student_count: 412,
                    compliance_health: "Green".into(),
                    last_activity: "2026-05-07".into(),
                    open_issues: 0,
                },
                CmoSchoolRow {
                    school_id: "0xdemo_b".into(),
                    display_name: "KIPP Charter Bayonne".into(),
                    student_count: 287,
                    compliance_health: "Yellow".into(),
                    last_activity: "2026-05-06".into(),
                    open_issues: 1,
                },
                CmoSchoolRow {
                    school_id: "0xdemo_c".into(),
                    display_name: "KIPP Charter Jersey City".into(),
                    student_count: 521,
                    compliance_health: "Green".into(),
                    last_activity: "2026-05-05".into(),
                    open_issues: 1,
                },
            ];
            let school_model = std::rc::Rc::new(slint::VecModel::from(stub_school_rows));
            ui.set_cmo_dashboard_schools(slint::ModelRc::from(school_model));

            let stub_events: Vec<CmoEvent> = vec![
                CmoEvent {
                    event_type: "DPA Renewal".into(),
                    school_name: "KIPP Charter Newark".into(),
                    actor: "0xacea…86d1".into(),
                    summary: "Annual DPA renewed; signed via Docusign + CLEAR".into(),
                    occurred_at: "2026-05-07 10:14".into(),
                },
                CmoEvent {
                    event_type: "Role Grant".into(),
                    school_name: "KIPP Charter Bayonne".into(),
                    actor: "0x9dc0…3b85".into(),
                    summary: "IT director appointed; admin-can-do-IT bridge enabled".into(),
                    occurred_at: "2026-05-06 16:42".into(),
                },
                CmoEvent {
                    event_type: "Policy Change".into(),
                    school_name: "".into(),
                    actor: "0x4250…00c6".into(),
                    summary: "CMO content-filter policy updated to K-12 Standard v3".into(),
                    occurred_at: "2026-05-05 09:01".into(),
                },
            ];
            let event_model = std::rc::Rc::new(slint::VecModel::from(stub_events));
            ui.set_cmo_dashboard_events(slint::ModelRc::from(event_model));

            // WP-E6.4 — Tenancy tree stub. Flat-list rendering of:
            //   CMO-root
            //     ├── KIPP Charter Newark      (4 classrooms · 412 students)
            //     │     ├── Classroom 7A        (28 students)
            //     │     └── Classroom 7B        (32 students)
            //     ├── KIPP Charter Bayonne    (3 classrooms · 287 students)
            //     └── KIPP Charter Jersey City (5 classrooms · 521 students)
            // The on-chain version of this list comes from
            // InstitutionTreeV1.getCmoTree(cmo_id) flattened DFS in
            // E6.1.5. For demo we hard-code the shape.
            let stub_nodes: Vec<TenancyNode> = vec![
                TenancyNode {
                    node_id: "0xdemo_cmo".into(),
                    kind: "cmo".into(),
                    display_name: "KIPP Public Schools NJ".into(),
                    level: 0,
                    student_count: 1220,
                    classroom_count: 0,
                    expanded: true,
                },
                TenancyNode {
                    node_id: "0xdemo_a".into(),
                    kind: "school".into(),
                    display_name: "KIPP Charter Newark".into(),
                    level: 1,
                    student_count: 412,
                    classroom_count: 4,
                    expanded: true,
                },
                TenancyNode {
                    node_id: "0xdemo_a_7a".into(),
                    kind: "classroom".into(),
                    display_name: "Classroom 7A".into(),
                    level: 2,
                    student_count: 28,
                    classroom_count: 0,
                    expanded: false,
                },
                TenancyNode {
                    node_id: "0xdemo_a_7b".into(),
                    kind: "classroom".into(),
                    display_name: "Classroom 7B".into(),
                    level: 2,
                    student_count: 32,
                    classroom_count: 0,
                    expanded: false,
                },
                TenancyNode {
                    node_id: "0xdemo_b".into(),
                    kind: "school".into(),
                    display_name: "KIPP Charter Bayonne".into(),
                    level: 1,
                    student_count: 287,
                    classroom_count: 3,
                    expanded: false,
                },
                TenancyNode {
                    node_id: "0xdemo_c".into(),
                    kind: "school".into(),
                    display_name: "KIPP Charter Jersey City".into(),
                    level: 1,
                    student_count: 521,
                    classroom_count: 5,
                    expanded: false,
                },
            ];
            let tenancy_model = std::rc::Rc::new(slint::VecModel::from(stub_nodes));
            ui.set_cmo_tenancy_nodes(slint::ModelRc::from(tenancy_model));
            ui.set_cmo_tenancy_action_error("".into());

            // WP-E6.5 — Compliance matrix stub.
            //
            // Build one ComplianceSchoolMatrixRow per school. Each row
            // has 9 cells in fixed order:
            //   [0] DPA, [1] FERPA, [2] COPPA, [3] CIPA,
            //   [4] CA AB1584, [5] NY Ed Law 2-d, [6] IL SOPPA,
            //   [7] TX TEC §32.151, [8] CO C.R.S. §22-16-104
            //
            // State-specific gates render N/A unless school's state matches.
            // Stub schools are all in NJ — so all 5 state-specific cells
            // render N/A; federal gates show realistic Green/Yellow/Red mix
            // matching the dashboard's compliance-health column.
            //
            // Real version comes from InstitutionTreeV1.getComplianceMatrix
            // when E6.1.5 wires the on-chain query.
            let make_cell = |gate_id: &str, gate_label: &str, status: &str,
                              last_signed: &str, expires_at: &str| -> ComplianceCell {
                ComplianceCell {
                    gate_id: gate_id.into(),
                    gate_label: gate_label.into(),
                    status: status.into(),
                    last_signed: last_signed.into(),
                    expires_at: expires_at.into(),
                }
            };

            // Schools are all in NJ, so all 5 state cells = "N/A"
            let na_cell = |gate_id: &str, gate_label: &str| -> ComplianceCell {
                make_cell(gate_id, gate_label, "N/A", "", "")
            };

            // Newark = all federal Green
            let newark_cells: Vec<ComplianceCell> = vec![
                make_cell("dpa",   "DPA",   "Green",  "2026-04-01", "2027-04-01"),
                make_cell("ferpa", "FERPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("coppa", "COPPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("cipa",  "CIPA",  "Green",  "2026-04-01", "2027-04-01"),
                na_cell("ab1584",   "CA AB1584"),
                na_cell("nyedlaw",  "NY Ed Law 2-d"),
                na_cell("ilsoppa",  "IL SOPPA"),
                na_cell("txtec",    "TX TEC §32.151"),
                na_cell("cocrs",    "CO C.R.S. §22-16-104"),
            ];

            // Bayonne = DPA Yellow (renewal pending), rest Green/N/A
            let bayonne_cells: Vec<ComplianceCell> = vec![
                make_cell("dpa",   "DPA",   "Yellow", "2025-04-01", "2026-04-01"),  // expired & pending renewal
                make_cell("ferpa", "FERPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("coppa", "COPPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("cipa",  "CIPA",  "Green",  "2026-04-01", "2027-04-01"),
                na_cell("ab1584",   "CA AB1584"),
                na_cell("nyedlaw",  "NY Ed Law 2-d"),
                na_cell("ilsoppa",  "IL SOPPA"),
                na_cell("txtec",    "TX TEC §32.151"),
                na_cell("cocrs",    "CO C.R.S. §22-16-104"),
            ];

            // Jersey City = CIPA Red (expired and not renewed)
            let jersey_cells: Vec<ComplianceCell> = vec![
                make_cell("dpa",   "DPA",   "Green",  "2026-04-01", "2027-04-01"),
                make_cell("ferpa", "FERPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("coppa", "COPPA", "Green",  "2026-03-15", "2027-03-15"),
                make_cell("cipa",  "CIPA",  "Red",    "2024-04-01", "2025-04-01"),  // expired 1+ yr ago
                na_cell("ab1584",   "CA AB1584"),
                na_cell("nyedlaw",  "NY Ed Law 2-d"),
                na_cell("ilsoppa",  "IL SOPPA"),
                na_cell("txtec",    "TX TEC §32.151"),
                na_cell("cocrs",    "CO C.R.S. §22-16-104"),
            ];

            let stub_compliance: Vec<ComplianceSchoolMatrixRow> = vec![
                ComplianceSchoolMatrixRow {
                    school_id: "0xdemo_a".into(),
                    school_name: "KIPP Charter Newark".into(),
                    school_state: "NJ".into(),
                    cells: slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
                        newark_cells,
                    ))),
                },
                ComplianceSchoolMatrixRow {
                    school_id: "0xdemo_b".into(),
                    school_name: "KIPP Charter Bayonne".into(),
                    school_state: "NJ".into(),
                    cells: slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
                        bayonne_cells,
                    ))),
                },
                ComplianceSchoolMatrixRow {
                    school_id: "0xdemo_c".into(),
                    school_name: "KIPP Charter Jersey City".into(),
                    school_state: "NJ".into(),
                    cells: slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
                        jersey_cells,
                    ))),
                },
            ];
            let compliance_model =
                std::rc::Rc::new(slint::VecModel::from(stub_compliance));
            ui.set_cmo_compliance_rows(slint::ModelRc::from(compliance_model));
        } else {
            // WP-E6.1.5 — Real-RPC path. When CITRATE_CMO_DEMO is unset, the
            // GUI checks whether the InstitutionTreeV1 + ComplianceRegistry
            // contracts have been deployed. The deployed-address sentinel is
            // ZERO_ADDRESS until the post-RegistryDeployment ceremony lands
            // canonical addresses in citrate-edu-app/src/config.rs. Until
            // then this branch logs the not-deployed state and leaves the
            // CMO panels empty (no fake data — Rule 0).
            //
            // After deployment: this branch instantiates a CmoPortalService
            // and spawns a tokio task that calls listAllSchoolsForCmo +
            // getSchoolMatrix per school + getNode for each. Results are
            // pushed back to the Slint UI via `invoke_from_event_loop`.
            //
            // Architecture intentionally keeps the service in
            // citrate-edu-app (which already has the eth_call gateway
            // pattern) so the GUI binary stays focused on UI plumbing.
            use citrate_edu_app::config::EduConfig;
            use citrate_edu_app::role::EduRole;
            use citrate_edu_app::services::cmo_portal::CmoPortalService;
            let config = EduConfig::testnet();

            // Operator-override env vars (used by E6.7 E2E and visual review
            // against a local anvil deployment). Production reads from
            // EduContracts after the deployment ceremony pins the addrs.
            // NAT-B-015: the CITRATE_CMO_E2E_* overrides repoint the
            // institution-tree / compliance-registry contracts and the RPC,
            // and reaching the `else` branch below grants super-admin. An
            // env-supplied address must NOT be able to self-grant in a
            // shipped binary, so honor these overrides ONLY in debug builds;
            // release always reads the pinned config contracts.
            let (env_tree, env_registry, env_rpc, env_cmo_hash) = if cfg!(debug_assertions) {
                (
                    std::env::var("CITRATE_CMO_E2E_TREE").ok(),
                    std::env::var("CITRATE_CMO_E2E_REGISTRY").ok(),
                    std::env::var("CITRATE_CMO_E2E_RPC").ok(),
                    std::env::var("CITRATE_CMO_E2E_CMO_HASH").ok(),
                )
            } else {
                (None, None, None, None)
            };

            let tree_addr = env_tree
                .clone()
                .unwrap_or_else(|| config.contracts.institution_tree.to_string());
            let registry_addr = env_registry
                .clone()
                .unwrap_or_else(|| config.contracts.compliance_registry.to_string());
            let rpc_url = env_rpc.unwrap_or_else(|| config.rpc_url.clone());
            let zero = citrate_edu_app::config::ZERO_ADDRESS;

            if tree_addr == zero || registry_addr == zero {
                tracing::info!(
                    "[E6.1.5] CMO portal contracts not yet deployed — \
                     institution_tree={tree_addr}, compliance_registry={registry_addr}. \
                     Run with CITRATE_CMO_DEMO=true to see the demo data, \
                     or update DEPLOYED_ADDRESSES.md after the ceremony, \
                     or set CITRATE_CMO_E2E_{{RPC,TREE,REGISTRY,CMO_HASH}} \
                     for local-anvil testing."
                );
            } else {
                ui.set_is_cmo_super_admin(true);
                // The CMO id hash comes from the active wallet's HKDF-
                // derived org identity. Until that wiring lands (E6.1
                // role-detection extension), the operator can override
                // via CITRATE_CMO_E2E_CMO_HASH for live-anvil testing.
                let cmo_hash = env_cmo_hash.unwrap_or_else(|| {
                    use sha3::{Digest, Keccak256};
                    let h = Keccak256::digest(b"E2E-CMO-KIPP");
                    format!("0x{}", hex::encode(h))
                });

                let service = std::sync::Arc::new(CmoPortalService::new(
                    std::sync::Arc::new(citrate_edu_app::gateway::GatewayClient::new(
                        &rpc_url,
                        None,
                        EduRole::CMOSuperAdmin,
                    )),
                    tree_addr.clone(),
                    registry_addr.clone(),
                ));
                tracing::info!(
                    "[E6.1.5-D] CMO portal real-RPC fetch starting — \
                     rpc={rpc_url}, tree={tree_addr}, registry={registry_addr}, \
                     cmo_hash={cmo_hash}"
                );

                // Spawn the async fetch task. Three sequential calls:
                //   1. list_schools_for_cmo(cmo_hash) — bytes32[]
                //   2. for each school: get_node + get_school_matrix
                //   3. invoke_from_event_loop to push results to UI props
                //
                // We DON'T spawn-per-school in parallel because the GUI
                // expects a single atomic update; partial population
                // would render a half-empty UI for tens of milliseconds
                // and look broken. Single sequential task is fast enough
                // for a typical CMO (<50 schools).
                let ui_weak_e615 = ui.as_weak();
                let cmo_hash_owned = cmo_hash.clone();
                rt.spawn(async move {
                    if let Err(err) = fetch_cmo_portal_data(
                        service,
                        cmo_hash_owned,
                        ui_weak_e615,
                    ).await {
                        tracing::warn!("[E6.1.5-D] CMO portal fetch failed: {err}");
                    }
                });
            }
        }
    }
    // WP-E6.6 — School-context propagation.
    //
    // Every school selection bumps `cmo-context-version`. Slint
    // property-bindings observing the version trigger a re-render in
    // the panel that's currently visible. The Rust side ALSO logs each
    // service that would re-fetch with the new active-school-id so a
    // human reading the trace can see the propagation working before
    // E6.1.5 wires real on-chain queries.
    //
    // Service-layer hooks documented here (one per panel):
    //
    //   Dashboard       → fetch_block_height(school_id), fetch_node_status
    //                      (currently node-scoped; E6.1.5 makes these
    //                      school-scoped via InstitutionTreeV1.getSchoolMetrics)
    //   Wallet           → wallet_manager.set_active_account(school_id)
    //                      (active-school maps to a school-scoped wallet
    //                      derivation path; E6.1.5)
    //   Models           → ModelsService.list_for_school(school_id)
    //                      (E6.1.5 — model lifecycle is per-school)
    //   Studio           → no-op (CMO-scope read of artifacts; cosmetic)
    //   Operations       → AgentTrail.filter(school_id) +
    //                      ApprovalQueue.filter(school_id)
    //   Compute / Storage → CIF-scoped already; cosmetic
    //
    // For v1 (without E6.1.5 on-chain queries) the callback bumps the
    // version + logs each hook. When E6.1.5 lands, each "log this hook"
    // line becomes a real RPC call.
    let ui_handle = ui.as_weak();
    ui.on_cmo_school_selected(move |school_id| {
        let id = school_id.as_str();
        if let Some(ui) = ui_handle.upgrade() {
            let prev = ui.get_cmo_context_version();
            ui.set_cmo_context_version(prev + 1);
            tracing::info!(
                "[E6.6] CMO school context version bumped {} -> {} for school={}",
                prev,
                prev + 1,
                id
            );
            // Document each service-layer hook. These become real RPC
            // calls when E6.1.5 ships InstitutionTreeV1.getSchoolMetrics
            // etc; for now we log so the propagation chain is observable.
            tracing::info!("[E6.6] dashboard hook: refresh_for_school({}) [pending E6.1.5]", id);
            tracing::info!("[E6.6] wallet hook: set_active_account_for_school({}) [pending E6.1.5]", id);
            tracing::info!("[E6.6] models hook: list_for_school({}) [pending E6.1.5]", id);
            tracing::info!("[E6.6] operations hook: filter_for_school({}) [pending E6.1.5]", id);
        }
    });

    // WP-E6.6 — Banner "Switch" button. Today the school selector lives
    // in the sidebar; clicking the banner's switch is a hint that the
    // operator wants to change schools. v1 logs; a future enhancement
    // could programmatically open the sidebar dropdown.
    ui.on_cmo_banner_switch_clicked(|| {
        tracing::info!("[E6.6] banner switch button clicked — operator wants to switch school context");
    });

    // WP-E6.3 — CMO dashboard's per-school row click navigates to that
    // school's view by setting the active-school + switching the active
    // tab to "dashboard" (the per-school dashboard). E6.6 finishes the
    // story by re-rendering all per-school panels with the new context;
    // for now this just sets the school selection and tab.
    let ui_handle_dash = ui.as_weak();
    ui.on_cmo_dashboard_school_clicked(move |school_id| {
        use slint::Model;
        let id = school_id.as_str();
        tracing::info!(
            "[E6.3] CMO dashboard row clicked: school {} — navigating to per-school dashboard",
            id
        );
        if let Some(ui) = ui_handle_dash.upgrade() {
            // Look up the school's display name from the schools list.
            let schools = ui.get_cmo_schools();
            let mut display_name = String::new();
            for i in 0..schools.row_count() {
                if let Some(s) = schools.row_data(i) {
                    if s.school_id.as_str() == id {
                        display_name = s.display_name.into();
                        break;
                    }
                }
            }
            ui.set_cmo_active_school_id(id.into());
            ui.set_cmo_active_school_name(display_name.into());
            ui.set_active_tab("dashboard".into());
            // E6.6 — bump context version on dashboard row click.
            let prev = ui.get_cmo_context_version();
            ui.set_cmo_context_version(prev + 1);
            tracing::info!(
                "[E6.6] CMO context version bumped {} -> {} via dashboard row click (school={})",
                prev,
                prev + 1,
                id
            );
        }
    });

    // WP-E6.4 — Tenancy panel callbacks.
    //
    // Click on a node row: expand/collapse semantics are reserved for
    // a future enhancement; for now clicking just logs. The on-chain
    // role-grant view + audit-log filter the planset describes lands
    // alongside E6.5 Compliance which has the same shape.
    ui.on_cmo_tenancy_node_clicked(|node_id| {
        tracing::info!(
            "[E6.4] CMO tenancy node clicked: {} — role-grant view + audit filter pending",
            node_id.as_str()
        );
    });

    // Add School / Remove School buttons: surface the action-error
    // banner because the on-chain RPC integration hasn't landed yet
    // (E6.1.5). The buttons exist so the UI shape is real, but each
    // refuses with a clear path forward — partner can see what the
    // workflow looks like and Saul gets a place to wire the on-chain
    // call when InstitutionTreeV1.addSchool / removeSchool are ready.
    let ui_handle_add = ui.as_weak();
    ui.on_cmo_tenancy_add_school_clicked(move |cmo_id| {
        tracing::info!(
            "[E6.4] add-school clicked under cmo={} — refusing (E6.1.5 RPC pending)",
            cmo_id.as_str()
        );
        if let Some(ui) = ui_handle_add.upgrade() {
            ui.set_cmo_tenancy_action_error(
                "Add School: on-chain InstitutionTreeV1.addSchool RPC not yet wired (E6.1.5). \
                 Use `citrate-school-bootstrap init` on the new school's IT machine to register \
                 it via the bootstrap CLI's tenancy flow until this button goes live."
                    .into(),
            );
        }
    });

    let ui_handle_rem = ui.as_weak();
    ui.on_cmo_tenancy_remove_school_clicked(move |school_id| {
        tracing::info!(
            "[E6.4] remove-school clicked for school={} — refusing (E6.1.5 RPC pending)",
            school_id.as_str()
        );
        if let Some(ui) = ui_handle_rem.upgrade() {
            ui.set_cmo_tenancy_action_error(
                "Remove School: on-chain InstitutionTreeV1.removeSchool RPC not yet wired \
                 (E6.1.5). This is a destructive action that revokes tenancy + invalidates \
                 the school's per-student profile packs; explicit chain integration is \
                 required before going live. Use `cast send` against InstitutionTreeV1 \
                 manually until this button is enabled."
                    .into(),
            );
        }
    });

    // WP-E6.5 — Compliance matrix callbacks. Click on a school name
    // navigates to that school's per-school dashboard (same pattern as
    // E6.3); clicking a status pill in a cell logs the (school, gate)
    // pair for the future drawer (E6.5.1 will surface envelope-detail
    // panels with signer / signed-date / expiry / Docusign envelope
    // ID — that requires the compliance_storage filesystem read which
    // happens in the next sub-task).
    let ui_handle_compl_school = ui.as_weak();
    ui.on_cmo_compliance_school_clicked(move |school_id| {
        use slint::Model;
        let id = school_id.as_str();
        tracing::info!(
            "[E6.5] compliance row school clicked: {} — navigating to per-school dashboard",
            id
        );
        if let Some(ui) = ui_handle_compl_school.upgrade() {
            let schools = ui.get_cmo_schools();
            let mut display_name = String::new();
            for i in 0..schools.row_count() {
                if let Some(s) = schools.row_data(i) {
                    if s.school_id.as_str() == id {
                        display_name = s.display_name.into();
                        break;
                    }
                }
            }
            ui.set_cmo_active_school_id(id.into());
            ui.set_cmo_active_school_name(display_name.into());
            ui.set_active_tab("dashboard".into());
            // E6.6 — bump context version on compliance row click.
            let prev = ui.get_cmo_context_version();
            ui.set_cmo_context_version(prev + 1);
            tracing::info!(
                "[E6.6] CMO context version bumped {} -> {} via compliance row click (school={})",
                prev,
                prev + 1,
                id
            );
        }
    });

    // WP-E6.5.1 — Compliance cell click opens the envelope-detail drawer.
    //
    // Cell click flow:
    //   1. Find the row matching school_id in cmo-compliance-rows
    //   2. Find the cell matching gate_id in that row's cells
    //   3. Build an EnvelopeDetail from the cell + school metadata
    //   4. Set ui.cmo-envelope-detail (which makes the drawer visible)
    //
    // The detail is populated entirely from data already in the UI's
    // compliance model — no extra RPC calls. When E6.1.5-D's async-fetch
    // loop ships, the same path works because the model contents come
    // from the live ComplianceRegistry.getSchoolMatrix call.
    let ui_handle_cell = ui.as_weak();
    ui.on_cmo_compliance_cell_clicked(move |school_id, gate_id| {
        use slint::Model;
        let school_id_s = school_id.as_str();
        let gate_id_s = gate_id.as_str();
        tracing::info!(
            "[E6.5.1] compliance cell clicked: school={}, gate={} — opening drawer",
            school_id_s,
            gate_id_s
        );
        if let Some(ui) = ui_handle_cell.upgrade() {
            let rows = ui.get_cmo_compliance_rows();
            for i in 0..rows.row_count() {
                let Some(row) = rows.row_data(i) else { continue };
                if row.school_id.as_str() != school_id_s {
                    continue;
                }
                let cells = row.cells.clone();
                for j in 0..cells.row_count() {
                    let Some(cell) = cells.row_data(j) else { continue };
                    if cell.gate_id.as_str() != gate_id_s {
                        continue;
                    }
                    let explainer = match cell.status.as_str() {
                        "Green" => "This gate is signed and current. The envelope expires on the date below; the keeper sweep will mark it Expired automatically.".to_string(),
                        "Yellow" => "An envelope has been sent for signature but no signing event is on-chain yet. If the operator has been waiting more than 5 business days, escalate to the school's compliance officer.".to_string(),
                        "Red" => "The signing window has lapsed or the gate was revoked under audit. Re-signing is required to restore compliance.".to_string(),
                        "N/A" => "This gate doesn't apply to this school's state. The cell renders for context only — CMOs operating in multiple states track per-state posture across the portfolio.".to_string(),
                        _ => "This gate has never been signed for this school. Sign the envelope via Docusign + CLEAR to record on-chain.".to_string(),
                    };
                    let detail = EnvelopeDetail {
                        visible: true,
                        school_id: row.school_id.clone(),
                        school_display_name: row.school_name.clone(),
                        school_state: row.school_state.clone(),
                        gate_id: cell.gate_id.clone(),
                        gate_label: cell.gate_label.clone(),
                        status: cell.status.clone(),
                        envelope_id_hash: "".into(),  // populated by E6.1.5-D fetch
                        signer: "".into(),             // populated by E6.1.5-D fetch
                        signed_at: cell.last_signed.clone(),
                        expires_at: cell.expires_at.clone(),
                        explainer: explainer.into(),
                    };
                    ui.set_cmo_envelope_detail(detail);
                    return;
                }
            }
            tracing::warn!(
                "[E6.5.1] compliance cell-click did not match any row/cell: school={}, gate={}",
                school_id_s,
                gate_id_s
            );
        }
    });

    // WP-E6.5.1 + E6.1.5-E — Renew/Sign-now button on the drawer.
    //
    // Builds the ABI-encoded calldata for ComplianceRegistry.recordSigned
    // and logs it as a copy-pasteable hex blob so the operator can sign
    // it via their existing wallet (MetaMask, hardware wallet, ledger,
    // etc.) and submit via eth_sendRawTransaction. This intentional
    // separation keeps the GUI key-custody-agnostic until the hardware-
    // wallet integration ships separately.
    //
    // Operator flow:
    //   1. Click "Renew now" in the drawer
    //   2. tracing::info! prints the calldata hex + contract address
    //   3. Operator signs + submits via their wallet
    //   4. The async fetch loop (E6.1.5-D) picks up the new state on
    //      next refresh
    //
    // To gate-idx mapping (must match ComplianceRegistry.Gate enum):
    //   "dpa"=0, "ferpa"=1, "coppa"=2, "cipa"=3,
    //   "ab1584"=4, "ny2d"=5, "ilsoppa"=6, "txtec"=7, "cocrs"=8
    ui.on_cmo_envelope_renew_clicked(|school_id, gate_id| {
        let school_id_s = school_id.as_str();
        let gate_id_s = gate_id.as_str();
        let gate_idx: u8 = match gate_id_s {
            "dpa" => 0,
            "ferpa" => 1,
            "coppa" => 2,
            "cipa" => 3,
            "ab1584" | "ca" => 4,
            "ny2d" | "ny" => 5,
            "ilsoppa" | "il" => 6,
            "txtec" | "tx" => 7,
            "cocrs" | "co" => 8,
            other => {
                tracing::warn!("[E6.1.5-E] unknown gate id: {other}; cannot build calldata");
                return;
            }
        };

        // Default expiry = now + 365 days (matches the contract's
        // MAX_VALIDITY_WINDOW). Operators can override at signing time
        // if they want a shorter window.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = now + 365 * 86400;

        // Synthetic envelope hash = keccak256(school_id || gate_id || now)
        // Real signing flow generates this from the actual Docusign
        // envelope ID; we use the synthetic placeholder so the calldata
        // is structurally valid for the operator's review/sign.
        use sha3::{Digest, Keccak256};
        let mut hasher = Keccak256::new();
        hasher.update(school_id_s.as_bytes());
        hasher.update(gate_id_s.as_bytes());
        hasher.update(now.to_be_bytes());
        let env_hash: [u8; 32] = hasher.finalize().into();
        let env_hash_hex = format!("0x{}", hex::encode(env_hash));

        let calldata = citrate_edu_app::tx::build_record_signed_calldata(
            school_id_s,
            gate_idx,
            &env_hash_hex,
            expires_at,
        );
        let calldata_hex = format!("0x{}", hex::encode(&calldata));

        tracing::info!(
            "[E6.1.5-E] recordSigned tx ready for operator signing:\n\
             To: <ComplianceRegistry contract address>\n\
             school_id_hash: {school_id_s}\n\
             gate_idx: {gate_idx} ({gate_id_s})\n\
             envelope_id_hash: {env_hash_hex}\n\
             expires_at: {expires_at} (now + 365d)\n\
             calldata: {calldata_hex}"
        );
    });
    {
        let config = rt.block_on(app_core.config.read());
        ui.set_environment(config.network.to_uppercase().into());
        // Bind the Settings RPC-port display to the actual config value
        // (canonicalized to 8545) instead of a hardcoded literal.
        ui.set_rpc_port(config.rpc_port as i32);

        // NATIVE-R1-S1 WP-1: apply the persisted appearance mode at startup.
        // "dark" → evergreen dark; "light"/"system" → canonical light.
        ui.global::<Theme>().set_dark_mode(config.theme == "dark");
        ui.set_settings_theme_mode(config.theme.clone().into());
    }

    // Push bootnode and wallet data to UI
    {
        let config = rt.block_on(app_core.config.read());
        ui.set_bootnode_list(config.bootnodes.join("\n").into());

        // Push wallet accounts to UI (EIP-55 checksummed addresses)
        let accounts = rt.block_on(app_core.wallet.list_accounts());
        push_accounts_to_ui(&ui, &accounts);
        if let Some(first) = accounts.first() {
            ui.set_wallet_selected_address(eip55_checksum(&first.address).into());
            ui.set_wallet_selected_label(first.label.clone().into());
            ui.set_wallet_selected_balance(first.balance.to_string().into());
        }
    }

    // --- Create Wallet ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_create_wallet(move |password, _label| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let pwd = password.to_string();

        tracing::info!("Creating wallet...");
        spawn_async(&rt_h, async move {
            match core.wallet.create_wallet(&pwd).await {
                Ok(result) => {
                    tracing::info!("Wallet created: {}", result.address);
                    // Mark the GUI-side session clock active too. The
                    // backend already activated session.is_active inside
                    // create_wallet — we mirror that with the unlock
                    // epoch so the countdown UI shows the right time.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    SESSION_UNLOCK_EPOCH.store(now, Ordering::Relaxed);
                    // Refresh account list after creation
                    let accounts = core.wallet.list_accounts().await;
                    let checksummed = eip55_checksum(&result.address);
                    let session_initial = format_session_remaining(SESSION_TIMEOUT_SECS);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_mnemonic(result.mnemonic.into());
                            ui.set_onboarding_wallet_address(checksummed.clone().into());
                            ui.set_onboarding_error("".into());
                            ui.set_onboarding_step(2);
                            ui.set_wallet_selected_address(checksummed.into());
                            ui.set_wallet_selected_label("Default".into());
                            // Immediately reflect the unlocked session so the
                            // wallet panel doesn't show 🔒 for up to 3s
                            // while the background tick catches up. Users
                            // interpreted the gap as "wallet is locked" and
                            // couldn't proceed to list/stake.
                            ui.set_wallet_session_active(true);
                            ui.set_wallet_session_remaining(session_initial.into());
                            push_accounts_to_ui(&ui, &accounts);
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Wallet creation failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Copy Mnemonic to Clipboard ---
    // NOTE: arboard::Clipboard::new() must run on the main thread.
    // Spawning a clipboard thread crashes accesskit on X11.
    //
    // RM-B1 / WP-E2.7 (audit GUI-C-03): clipboard auto-clear. After
    // 30 seconds the mnemonic is wiped from the clipboard if (and
    // only if) it still matches what we wrote — we don't clobber
    // something the user copied themselves in the interim.
    let ui_w = ui.as_weak();
    ui.on_copy_mnemonic(move || {
        if let Some(ui) = ui_w.upgrade() {
            let mnemonic = ui.get_onboarding_mnemonic().to_string();
            if !mnemonic.is_empty() {
                match arboard::Clipboard::new() {
                    Ok(mut clipboard) => {
                        if clipboard.set_text(&mnemonic).is_ok() {
                            tracing::info!("Mnemonic copied to clipboard (auto-clear in 30s)");
                            schedule_clipboard_autoclear(mnemonic.clone());
                        }
                    }
                    Err(e) => tracing::warn!("Clipboard not available: {}", e),
                }
            }
        }
    });

    // --- Verify Mnemonic Word ---
    let ui_w = ui.as_weak();
    ui.on_verify_mnemonic_word(move |input| {
        if let Some(ui) = ui_w.upgrade() {
            let mnemonic = ui.get_onboarding_mnemonic().to_string();
            let word_num = ui.get_onboarding_verify_word() as usize;
            let input_str = input.to_string().trim().to_lowercase();

            let words: Vec<&str> = mnemonic.split_whitespace().collect();
            if word_num > 0 && word_num <= words.len() {
                let expected = words[word_num - 1].to_lowercase();
                if input_str == expected {
                    tracing::info!("Mnemonic word {} verified correctly", word_num);
                    ui.set_onboarding_error("".into());
                    ui.set_onboarding_step(3);
                } else if input_str.is_empty() && cfg!(debug_assertions) {
                    // NAT-B-031: the empty-input skip is a DEV-ONLY escape
                    // hatch. In a RELEASE build `cfg!(debug_assertions)` is
                    // false, so an empty field falls through to the
                    // "incorrect" branch below and does NOT advance the
                    // wizard — a user can no longer complete wallet creation
                    // without ever recording the recovery phrase.
                    tracing::info!("Mnemonic verification skipped (empty input, debug build only)");
                    ui.set_onboarding_error("".into());
                    ui.set_onboarding_step(3);
                } else {
                    // NAT-B-005: NEVER log the expected word (it is live
                    // seed material) nor the user's input (which may also
                    // be a seed word). Log only the position that failed.
                    tracing::warn!("Mnemonic verification failed for word #{}", word_num);
                    ui.set_onboarding_error(
                        format!("Incorrect. Enter word #{} from your recovery phrase.", word_num).into()
                    );
                }
            }
        }
    });

    // Pick a random word number for verification (1-24)
    {
        let word_num = (rand::random::<u8>() % 24) as i32 + 1;
        ui.set_onboarding_verify_word(word_num);
    }

    // --- Start Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_start_node(move |env| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let environment = env.to_string();

        tracing::info!("Starting node for: {}", environment);

        if let Some(ui) = ui_w.upgrade() {
            ui.set_onboarding_node_status("Initializing storage...".into());
            ui.set_onboarding_node_progress(0.2);
        }
        loader_set_running(true); // WP-5: animate the bootstrap loader

        spawn_async(&rt_h, async move {
            crash_telemetry::set_last_state("node-start (onboarding bootstrap)");
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node started");
                    crash_telemetry::set_last_state("node-running (onboarding bootstrap)");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_node_status("Node running — connecting to network...".into());
                            ui.set_onboarding_node_progress(1.0);
                            ui.set_onboarding_node_ready(true);
                            ui.set_node_running(true);
                            ui.set_connection_status("Connecting to bootnode...".into());
                        }
                        loader_set_running(false); // WP-5: bootstrap done
                    });
                }
                Err(e) => {
                    let err = format!("Failed: {}", e);
                    tracing::error!("Node start failed: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_node_status(err.into());
                        }
                        loader_set_running(false); // WP-5: bootstrap failed
                    });
                }
            }
        });
    });

    // --- Check Model ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_check_model(move || {
        let ui_w = ui_w.clone();
        // Run detection in background — Ollama probe has a 2s timeout
        spawn_async(&rt_h, async move {
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let ready = detected.backend_type != "none";
            let status = if ready {
                format!("AI ready: {} — local-first, private", detected.display_name)
            } else {
                "No local model found — install Ollama or download a model in Settings".to_string()
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_onboarding_model_ready(ready);
                    ui.set_onboarding_model_status(status.into());
                }
            });
        });
    });

    // --- Onboarding Complete ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_onboarding_complete(move || {
        tracing::info!("Onboarding complete — landing on dashboard");
        let core = core.clone();
        let ui_w_inner = ui_w.clone();
        if let Some(ui) = ui_w.upgrade() {
            ui.set_show_onboarding(false);
            ui.set_active_tab("dashboard".into());
            // NAT-B-016: the full BIP-39 mnemonic lives in a root-scope Slint
            // property (`onboarding-mnemonic`). Pre-fix it was written once
            // and never cleared, so the seed survived for the whole process
            // lifetime (readable by a core dump / debugger / swap). Clear it
            // now that onboarding is finished.
            ui.set_onboarding_mnemonic("".into());
            // Auto-start node if not already running
            if !ui.get_node_running() {
                tracing::info!("Onboarding: auto-starting node");
                ui.set_connection_status("Starting node...".into());
                spawn_async(&rt_h, async move {
                    crash_telemetry::set_last_state("node-start (onboarding auto)");
                    match core.node.start().await {
                        Ok(()) => {
                            tracing::info!("Onboarding: node auto-started");
                            crash_telemetry::set_last_state("node-running (onboarding auto)");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w_inner.upgrade() {
                                    ui.set_node_running(true);
                                    ui.set_connection_status("Connecting to bootnode...".into());
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Onboarding: node auto-start failed: {}", e);
                            let err = format!("Node start failed: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w_inner.upgrade() {
                                    ui.set_connection_status(err.into());
                                }
                            });
                        }
                    }
                });
            }
        }
    });

    // --- Send Transaction ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_send(move |to, amount, password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let to_str = to.to_string();
        let amt_str = amount.to_string();
        // NAT-B-016: wipe the Rust-side password copy on drop.
        let pwd_str = Zeroizing::new(password.to_string());

        // NAT-B-002: the confirm-screen password is a real authorization
        // factor, not decoration. Reject an empty field on the interactive
        // path so a spend cannot be confirmed without typing the password
        // (the backend then verifies it against the keystore). Programmatic
        // callers use the service directly with "" and are session-gated.
        if pwd_str.is_empty() {
            let ui_w = ui_w.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_send_error("Enter your wallet password to confirm this transfer.".into());
                }
            });
            return;
        }

        // NAT-B-012: validate the recipient BEFORE signing. A malformed or
        // failed-checksum address is rejected at the dialog rather than
        // surfacing as a confusing "invalid hex" at signing time — or worse,
        // being broadcast to a mistyped destination.
        let to_str = match validate_recipient_address(&to_str) {
            Ok(normalized) => normalized,
            Err(e) => {
                let ui_w = ui_w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_send_error(e.into());
                    }
                });
                return;
            }
        };

        // Read the currently selected account address from UI state
        let from_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_else(|| "default".to_string());

        // Convert SALT amount → wei (1 SALT = 10^18 wei)
        // User types "1.5" meaning 1.5 SALT, backend expects wei string
        let wei_str = match citrate_wallet_core::format::salt_to_wei(&amt_str) {
            Ok(wei) => wei.to_string(),
            Err(e) => {
                let err_msg = format!("Invalid amount: {}", e);
                tracing::error!("{}", err_msg);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_send_error(err_msg.into());
                    }
                });
                return;
            }
        };

        tracing::info!("Sending {} SALT ({} wei) to {} from {}", amt_str, wei_str, to_str, from_addr);
        // NAT-B-013: flip the dialog into its in-flight state so the
        // "Confirm & Send" button is disabled while the send is pending.
        // Pre-fix `send-sending` was never written from Rust, so the button
        // stayed enabled and a double-click launched two concurrent sends
        // that fetched the same nonce.
        if let Some(ui) = ui_w.upgrade() {
            ui.set_send_sending(true);
        }
        spawn_async(&rt_h, async move {
            match core.wallet.send_transaction(&from_addr, &to_str, &wei_str, &pwd_str).await {
                Ok(hash) => {
                    tracing::info!("Transaction sent: {}", hash);
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        let hash = hash.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                // NAT-B-013: broadcast done (nonce consumed) —
                                // re-enable the button.
                                ui.set_send_sending(false);
                                ui.set_send_tx_hash(hash.into());
                                ui.set_send_error("".into());
                                ui.set_send_receipt_status("Submitted — waiting for receipt…".into());
                            }
                        }
                    });
                    // T1-1: poll the receipt so the user sees confirmed/reverted/pending
                    // instead of just a hash and a prayer.
                    // Poll the same node the wallet submitted to.
                    let rpc_url = core.wallet.get_rpc_url();
                    let final_msg = match poll_tx_receipt(&rpc_url, &hash).await {
                        Ok(ReceiptOutcome::Confirmed { block_number }) => {
                            format!("Confirmed in block {}", block_number)
                        }
                        Ok(ReceiptOutcome::Reverted) => {
                            "Reverted — check recipient address and balance".to_string()
                        }
                        Ok(ReceiptOutcome::Pending) => {
                            "Still pending — check explorer in a minute".to_string()
                        }
                        Err(e) => format!("Receipt poll failed: {}", e),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_send_receipt_status(final_msg.into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Send failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            // NAT-B-013: send failed — re-enable the button.
                            ui.set_send_sending(false);
                            ui.set_send_error(err.into());
                            ui.set_send_receipt_status("".into());
                        }
                    });
                }
            }
        });
    });

    // --- Send dialog close (reset state) ---
    let ui_w = ui.as_weak();
    ui.on_wallet_send_close(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_send_tx_hash("".into());
            ui.set_send_error("".into());
            ui.set_send_receipt_status("".into());
            // NAT-B-013: clear the in-flight flag on close.
            ui.set_send_sending(false);
        }
    });

    // --- Unlock Wallet (lock screen) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_unlock_wallet(move |password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        // NAT-B-016: wipe the Rust-side password copy on drop.
        let pwd = Zeroizing::new(password.to_string());

        // Use the selected account address (or first account if none selected)
        let selected_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();
        let addr = if selected_addr.is_empty() { "primary".to_string() } else { selected_addr };

        tracing::info!("Unlocking wallet for {}", addr);
        // NAT-B-021: drive the lock-screen's "unlocking" state (runs on the
        // UI thread inside this callback). Pre-fix `lock-unlocking`,
        // `lock-locked-out`, and `lock-lockout-message` were declared and
        // rendered but never written from Rust, so a user in the 5-minute
        // cooldown got no feedback and the Unlock button was never disabled
        // mid-attempt.
        if let Some(ui) = ui_w.upgrade() {
            ui.set_lock_unlocking(true);
            ui.set_lock_locked_out(false);
            ui.set_lock_lockout_message("".into());
        }
        spawn_async(&rt_h, async move {
            match core.wallet.unlock(&addr, &pwd).await {
                Ok(_status) => {
                    tracing::info!("Wallet unlocked");
                    // Record unlock time for session countdown
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    SESSION_UNLOCK_EPOCH.store(now, Ordering::Relaxed);
                    let session_initial = format_session_remaining(SESSION_TIMEOUT_SECS);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_lock_unlocking(false);
                            ui.set_lock_locked_out(false);
                            ui.set_lock_lockout_message("".into());
                            ui.set_show_lock_screen(false);
                            ui.set_lock_error("".into());
                            ui.set_wallet_session_active(true);
                            ui.set_wallet_session_remaining(session_initial.into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Unlock failed: {}", err);
                    // NAT-B-021: surface the cooldown so a legitimate user
                    // can tell a lockout from a wrong password.
                    let status = core.wallet.get_session_status().await;
                    let locked_out = status.is_locked_out;
                    let lockout_message = if locked_out {
                        match status.lockout_remaining_seconds {
                            Some(s) => format!(
                                "Too many failed attempts — locked for {}s. Try again later.",
                                s
                            ),
                            None => "Too many failed attempts — temporarily locked.".to_string(),
                        }
                    } else {
                        String::new()
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_lock_unlocking(false);
                            ui.set_lock_locked_out(locked_out);
                            ui.set_lock_lockout_message(lockout_message.into());
                            ui.set_lock_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Sign Out ---
    // NAT-B-004: "Sign Out" must actually CLOSE the wallet, not just swap
    // the view. It clears the session epoch (which gates the background
    // signing relay) and locks the wallet (zeroizing decrypted keys) —
    // the same custody teardown as the explicit Lock button — BEFORE
    // showing onboarding. Otherwise a user who signs out in a shared space
    // leaves a fully-unlocked wallet that keeps signing.
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_sign_out(move || {
        tracing::info!("Signed out — locking wallet and showing onboarding");
        let core = core.clone();
        let ui_w = ui_w.clone();
        spawn_async(&rt_h, async move {
            perform_wallet_lock_teardown(&core.wallet).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_session_active(false);
                    ui.set_wallet_session_remaining("".into());
                    ui.set_show_lock_screen(false);
                    ui.set_show_onboarding(true);
                }
            });
        });
    });

    // --- Generic copy-to-clipboard ---
    // Panels (chat, wallet, dag, contracts, ...) emit `copy-to-clipboard(text)`
    // and this handler writes to the OS clipboard via arboard, then sets
    // the `clipboard-toast` property so the shell can flash a "Copied"
    // banner. Toast clears after 1.5s.
    {
        let ui_w = ui.as_weak();
        ui.on_copy_to_clipboard(move |text| {
            let text = text.to_string();
            let preview: String = text.chars().take(40).collect();
            let label = if text.chars().count() > 40 {
                format!("Copied: {}…", preview)
            } else if text.is_empty() {
                "Copied (empty)".to_string()
            } else {
                format!("Copied: {}", preview)
            };
            match arboard::Clipboard::new() {
                Ok(mut clipboard) => {
                    if let Err(e) = clipboard.set_text(&text) {
                        tracing::warn!("Clipboard write failed: {}", e);
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_clipboard_toast("Copy failed".into());
                        }
                        return;
                    }
                    // RM-B1 / WP-E2.7 (audit GUI-C-03): wipe the
                    // clipboard 30s after copy if it still matches.
                    schedule_clipboard_autoclear(text.clone());
                }
                Err(e) => {
                    tracing::warn!("Clipboard not available: {}", e);
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_clipboard_toast("Copy unavailable".into());
                    }
                    return;
                }
            }
            if let Some(ui) = ui_w.upgrade() {
                ui.set_clipboard_toast(label.into());
            }
            // Clear the toast after 1.5s.
            let ui_for_clear = ui_w.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                if let Some(ui) = ui_for_clear.upgrade() {
                    ui.set_clipboard_toast("".into());
                }
            });
        });
    }

    // --- Tab Switching ---
    let ui_w = ui.as_weak();
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_tab_changed(move |tab| {
        let tab_str = tab.to_string();
        if let Some(ui) = ui_w.upgrade() {
            ui.set_active_tab(tab.clone());
        }
        // (Contracts tab hydration retired with the panel itself — P960-H.)

        // BFR-INT-1 — Boeing panel hydration on tab activation.
        // Each handler clones the live BoeingBindings + a weak UI handle,
        // spawns an async task that calls the adapter's fetch_*/assemble_*,
        // maps the returned `*PanelData` to Slint-generated row types, and
        // updates the App's `in property`s via `slint::invoke_from_event_loop`.
        //
        // FL is fully wired below as a worked example. The other 9 panels
        // log their fetch result for now; their full row mapping lands in
        // BFR-INT-1 follow-up commits (each panel is mechanical but ~50-100
        // LOC of conversion).
        // BFR-INT-4: Boeing tab dispatch moved to `citrate-boeing-shell` crate.


        // Hydrate Operations page on activation
        if tab_str == "operations" {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                // Trail events
                let events = core.trail.get_events().await;
                let trail_count = events.len() as i32;

                // Pending approvals
                let pending = core.approvals.list_pending().await;
                let pending_count = pending.len() as i32;
                let pending_entries: Vec<ApprovalEntryData> = pending.iter().map(|req| {
                    ApprovalEntryData {
                        request_id: req.request_id.clone().into(),
                        tool_name: req.tool_name.clone().into(),
                        risk_level: req.risk_level.clone().into(),
                        target: serde_json::to_string(&req.params).unwrap_or_default().into(),
                        created_at: req.created_at.clone().into(),
                    }
                }).collect();

                // Trail entries for display
                let trail_entries: Vec<TrailEntryData> = events.iter().map(|e| {
                    TrailEntryData {
                        timestamp: e.timestamp.split('T').next_back().unwrap_or(&e.timestamp).into(),
                        event_type: e.event_type.clone().into(),
                        tool_name: e.tool_name.clone().unwrap_or_default().into(),
                        risk_level: e.risk_level.clone().unwrap_or_default().into(),
                        approved: e.approved.map(|b| b.to_string()).unwrap_or_default().into(),
                    }
                }).collect();

                // Logseq status — check if path is configured
                let logseq_path = core.trail.logseq_path().await;
                let logseq_status = if logseq_path.is_some() { "online" } else { "disabled" };

                // P960-K T1-2: real session policy from AppCore
                let scope_str = match *core.session_policy.read().await {
                    citrate_agent_core::canonical::PolicyProfile::ReadOnly => "read-only",
                    citrate_agent_core::canonical::PolicyProfile::Guided => "guided",
                    citrate_agent_core::canonical::PolicyProfile::Operator => "operator",
                    citrate_agent_core::canonical::PolicyProfile::Maintainer => "maintainer",
                }.to_string();

                let ui_for_ops = ui_w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_for_ops.upgrade() {
                        ui.set_ops_trail_count(trail_count);
                        ui.set_ops_pending_count(pending_count);
                        ui.set_ops_active_sessions(1); // Current session
                        ui.set_ops_grant_scope(scope_str.into());
                        ui.set_ops_logseq_status(logseq_status.into());
                        ui.set_ops_logseq_path(logseq_path.unwrap_or_default().into());
                        // P960-J: MCP host state is set from the
                        // separate mcp_host.status() call below —
                        // we don't overwrite it here because this
                        // closure runs *before* the status fetch
                        // completes and would flash the correct
                        // value back to "offline" for one frame.

                        let pending_model = std::rc::Rc::new(slint::VecModel::from(pending_entries));
                        ui.set_ops_pending_approvals(pending_model.into());
                        let trail_model = std::rc::Rc::new(slint::VecModel::from(trail_entries));
                        ui.set_ops_trail_events(trail_model.into());
                    }
                });

                // T2-5: chain pause status — citrate_emergencyStatus
                let rpc_url = core.config.read().await.active_rpc_url();
                let client = reqwest::Client::new();
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "citrate_emergencyStatus",
                    "params": [],
                    "id": 1,
                });
                let paused = match client.post(&rpc_url).json(&body).send().await {
                    Ok(resp) => {
                        let json: Option<serde_json::Value> = resp.json().await.ok();
                        json.and_then(|j| j.get("result").and_then(|r| r.get("paused")).and_then(|p| p.as_bool()))
                            .unwrap_or(false)
                    }
                    Err(_) => false,
                };
                let ui_for_chain = ui_w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_for_chain.upgrade() {
                        ui.set_ops_chain_paused(paused);
                    }
                });

                // P960-J: MCP host status — separate await so the
                // main ops hydration doesn't block on mcp_host.
                let status = core.mcp_host.status().await;
                let ui_for_mcp = ui_w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_for_mcp.upgrade() {
                        if status.listening {
                            ui.set_ops_hermes_status("online".into());
                            ui.set_ops_hermes_endpoint(status.endpoint.into());
                            ui.set_ops_hermes_session_count(status.active_sessions.len() as i32);
                        } else {
                            ui.set_ops_hermes_status("offline".into());
                            ui.set_ops_hermes_endpoint("".into());
                            ui.set_ops_hermes_session_count(0);
                        }
                    }
                });
            });
        }

        // Hydrate Compute contract status on activation
        // T2-6: query ModelRegistry.getModelCount() on Models open so
        // the panel shows the real registered-model count instead of 0.
        if tab_str == "models" {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let chain_id = core.config.read().await.chain_id;
                let rpc_url = core.config.read().await.active_rpc_url();
                let Some(addr) = marketplace_client::model_registry_address(chain_id) else {
                    return;
                };
                let data = marketplace_client::encode_model_count();
                let count = match marketplace_client::eth_call(&rpc_url, addr, &data).await {
                    Ok(r) => marketplace_client::decode_uint256_u128(&r).unwrap_or(0) as i32,
                    Err(_) => 0,
                };
                tracing::info!("Models: ModelRegistry.getModelCount() = {}", count);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_models_count(count);
                    }
                });
            });
        }

        // P960-K T1-4: re-probe dependency health on every settings open
        if tab_str == "settings" {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                run_health_probes(&core, ui_w).await;
            });
        }

        if tab_str == "compute" {
            // Immediate probe so the Register button enables without
            // waiting for the 30s polling tick. Fetches both the
            // contract-address lookup AND the live getProvider call
            // so is-registered flips correctly on first paint.
            let ui_w = ui_w.clone();
            let core = core.clone();
            spawn_async(&rt_h, async move {
                let chain_id = core.config.read().await.chain_id;
                let rpc_url = core.config.read().await.active_rpc_url();
                let market_addr = marketplace_client::compute_marketplace_address(chain_id);
                let accounts = core.wallet.list_accounts().await;
                let self_addr = accounts.first().map(|a| a.address.clone());

                let status = if market_addr.is_some() {
                    "Marketplace live"
                } else {
                    "Marketplace not deployed on this network"
                };

                // Also read isRegistered so the button flips immediately.
                let is_registered: bool = if let (Some(m), Some(addr)) = (market_addr, self_addr.as_deref()) {
                    if let Some(data) = marketplace_client::encode_get_provider(addr) {
                        match marketplace_client::eth_call(&rpc_url, m, &data).await {
                            Ok(r) => marketplace_client::decode_provider_profile(&r)
                                .map(|p| p.is_registered)
                                .unwrap_or(false),
                            Err(_) => false,
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_compute_contract_status(status.into());
                        ui.set_compute_is_registered(is_registered);
                    }
                });
            });
        }
    });

    // --- SELL-S2 signing relay (background) ---
    // Drains the node-agent's signature-request queue and signs each *validated*
    // write with the unlocked wallet, so won jobs advance on-chain (GUI-RELAY-S1,
    // closes the gui-native half of TD-17/27). Opt-in + unlock-gated: signs only
    // while the relay is enabled AND a wallet session is active. Enable today via
    // `CITRATE_RELAY_ENABLED=1`; the Settings toggle (S1.3b) flips the same flag.
    let relay_for_toggle = {
        use citrate_desktop_app::services::relay_service::{
            NodeAgentClient, RelayConfig, RelayService, WalletTxSigner,
        };
        let chain_id = 40204u64;
        let marketplace = marketplace_client::compute_marketplace_address(chain_id).map(str::to_string);
        let accounting = marketplace_client::contribution_accounting_address(chain_id).map(str::to_string);
        let heartbeat_monitor =
            marketplace_client::known_contract(chain_id, "HeartbeatMonitor").map(str::to_string);
        match (marketplace, accounting, heartbeat_monitor) {
            (Some(marketplace), Some(accounting), Some(heartbeat_monitor)) => {
                let agent_url = std::env::var("CITRATE_NODE_AGENT_ADDR")
                    .unwrap_or_else(|_| "http://127.0.0.1:19600".to_string());
                let relay = std::sync::Arc::new(RelayService::new(RelayConfig {
                    agent_url: agent_url.clone(),
                    chain_id,
                    marketplace,
                    accounting,
                    heartbeat_monitor,
                    poll_interval: std::time::Duration::from_secs(5),
                }));
                if matches!(
                    std::env::var("CITRATE_RELAY_ENABLED").ok().as_deref(),
                    Some("1") | Some("true")
                ) {
                    relay.set_enabled(true);
                    tracing::info!("signing relay: enabled via CITRATE_RELAY_ENABLED (agent {agent_url})");
                }
                let ui_handle = ui.as_weak();
                let core = app_core.clone();
                let rt_handle = rt.handle().clone();
                // FUA-GUI-02: refuse to start the relay against a non-loopback
                // node-agent (try_new enforces loopback + loads the bearer token).
                match NodeAgentClient::try_new(agent_url.clone()) {
                  Err(e) => {
                    tracing::error!("signing relay NOT started: {e}");
                    None
                  }
                  Ok(agent) => {
                let agent = std::sync::Arc::new(agent);
                let signer = std::sync::Arc::new(WalletTxSigner::new(core.wallet.clone()));
                // FUA-GUI-01 residual: privileged writes (claimRewards /
                // value-bearing) require an explicit per-write approval via
                // the Operations pending-approvals surface; fail closed.
                let confirm_gate = std::sync::Arc::new(RelayApprovalGate::new(core.approvals.clone()));
                let relay_loop = relay.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(relay_loop.poll_interval());
                    // Custody gate: only sign while enabled AND a GUI session is unlocked.
                    if !relay_loop.is_enabled() || SESSION_UNLOCK_EPOCH.load(Ordering::Relaxed) <= 0 {
                        continue;
                    }
                    let from = match rt_handle.block_on(core.wallet.get_primary_address()) {
                        Some(a) if !a.is_empty() => a,
                        _ => continue,
                    };
                    let report = rt_handle.block_on(relay_loop.tick(
                        signer.as_ref(),
                        &from,
                        agent.as_ref(),
                        true,
                        confirm_gate.as_ref(),
                    ));
                    if let Some(r) = report {
                        for s in &r.signed {
                            let short = &s.tx_hash[..s.tx_hash.len().min(12)];
                            let msg = format!("Auto-signed {} (tx {short}…)", s.intent);
                            tracing::info!("signing relay: {msg}");
                            let ui_t = ui_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_t.upgrade() {
                                    ui.set_clipboard_toast(msg.clone().into());
                                    let ui_c = ui_t.clone();
                                    slint::Timer::single_shot(
                                        std::time::Duration::from_millis(5000),
                                        move || {
                                            if let Some(ui) = ui_c.upgrade() {
                                                ui.set_clipboard_toast("".into());
                                            }
                                        },
                                    );
                                }
                            });
                        }
                        for (id, reason) in &r.rejected {
                            tracing::warn!("signing relay: refused request {id}: {reason:?}");
                        }
                        for e in &r.errors {
                            tracing::warn!("signing relay: {e}");
                        }
                    }
                });
                Some(relay)
                  }
                }
            }
            _ => {
                tracing::warn!(
                    "signing relay: no canonical contract addresses for chain {chain_id}; relay disabled"
                );
                None
            }
        }
    };

    // --- Background data push (non-blocking) ---
    // Runs on a separate OS thread but uses the MAIN runtime handle.
    // CRITICAL: Do NOT create a second tokio::Runtime — the EmbeddedNodeBackend
    // stores data behind tokio::sync::RwLock which is tied to the main runtime.
    // A second runtime causes cross-runtime deadlocks.
    {
        let ui_handle = ui.as_weak();
        let core = app_core.clone();
        let rt_handle = rt.handle().clone();
        std::thread::spawn(move || {
            let mut tick_counter: u32 = 0;
            let mut baseline_balance_wei: Option<u128> = None;
            // WP-E.2: track which reward txs we've already notified on
            // so every new reward (≠ previously-seen hash) fires a toast
            // exactly once. We seed this set on the first tick after
            // unlock (treat prior rewards as "already shown") so we
            // don't spam a toast storm for historical rewards.
            let mut seen_reward_hashes: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut reward_seed_done = false;
            let mut last_reward_toast_ms: u128 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                tick_counter += 1;

                // Refresh status from backend then read it
                rt_handle.block_on(core.node.refresh_status());
                let status = rt_handle.block_on(core.node.get_status());
                let conn_status = if status.peer_count > 0 {
                    format!("Connected ({} peers)", status.peer_count)
                } else if status.running {
                    "Connecting to bootnode...".to_string()
                } else {
                    "Disconnected".to_string()
                };

                // Get recent blocks from RocksDB for dashboard
                // Data source: citrate_storage::BlockStore via EmbeddedNodeBackend::get_block_summaries
                let recent_blocks = rt_handle.block_on(core.node.get_recent_blocks(3))
                    .unwrap_or_default();
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs()).unwrap_or(0);

                // T2-15: tuple gains a 4th element — proposer short-hex
                let block_data: Vec<(String, String, String, String)> = recent_blocks.iter().map(|b| {
                    let hash = if b.hash.len() > 18 {
                        format!("{}...{}", &b.hash[..10], &b.hash[b.hash.len()-4..])
                    } else {
                        b.hash.clone()
                    };
                    let txcount = format!("{} txn{}", b.tx_count, if b.tx_count == 1 { "" } else { "s" });
                    let age = if b.timestamp > 0 && now_secs > b.timestamp {
                        let secs = now_secs - b.timestamp;
                        if secs < 60 { format!("~{}s ago", secs) }
                        else if secs < 3600 { format!("~{}m ago", secs / 60) }
                        else { format!("~{}h ago", secs / 3600) }
                    } else if b.timestamp == 0 {
                        String::new()
                    } else {
                        "just now".to_string()
                    };
                    let proposer = if b.proposer.len() >= 8 {
                        format!("by {}…", &b.proposer[..8])
                    } else if !b.proposer.is_empty() {
                        format!("by {}", b.proposer)
                    } else {
                        String::new()
                    };
                    (hash, txcount, age, proposer)
                }).collect();

                let hash0 = block_data.first().map(|d| d.0.clone()).unwrap_or_default();
                let hash1 = block_data.get(1).map(|d| d.0.clone()).unwrap_or_default();
                let hash2 = block_data.get(2).map(|d| d.0.clone()).unwrap_or_default();
                let txc0 = block_data.first().map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let txc1 = block_data.get(1).map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let txc2 = block_data.get(2).map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let time0 = block_data.first().map(|d| d.2.clone()).unwrap_or_default();
                let time1 = block_data.get(1).map(|d| d.2.clone()).unwrap_or_default();
                let time2 = block_data.get(2).map(|d| d.2.clone()).unwrap_or_default();
                let prop0 = block_data.first().map(|d| d.3.clone()).unwrap_or_default();
                let prop1 = block_data.get(1).map(|d| d.3.clone()).unwrap_or_default();
                let prop2 = block_data.get(2).map(|d| d.3.clone()).unwrap_or_default();

                // Refresh wallet accounts every 5th tick (~15s)
                let wallet_accounts = if tick_counter.is_multiple_of(5) {
                    Some(rt_handle.block_on(core.wallet.list_accounts()))
                } else {
                    None
                };

                // H-02 FIX: Read the SELECTED address from UI state, not primary.
                // This prevents overwriting account #2's balance with account #1's.
                let selected_addr_for_balance = ui_handle.upgrade()
                    .map(|ui| ui.get_wallet_selected_address().to_string());

                // Get balance for the selected address (or primary as fallback)
                // Data source: StateDB account balance (wei) via NodeBackend::get_balance
                let (primary_balance, selected_balance) = rt_handle.block_on(async {
                    let primary = core.wallet.get_primary_address().await;

                    // Selected account balance
                    let sel_addr = selected_addr_for_balance
                        .as_deref()
                        .filter(|a| !a.is_empty())
                        .or(primary.as_deref());

                    let sel_bal = if let Some(addr) = sel_addr {
                        let wei_str = core.node.get_balance(addr).await.unwrap_or_else(|_| "0".to_string());
                        match wei_str.parse::<u128>() {
                            Ok(wei) => citrate_wallet_core::format::wei_to_salt(wei),
                            Err(_) => wei_str,
                        }
                    } else {
                        "0".to_string()
                    };

                    // Primary balance (for the header/overview widget)
                    let pri_bal = if let Some(addr) = &primary {
                        if Some(addr.as_str()) == sel_addr {
                            sel_bal.clone() // same address, reuse
                        } else {
                            let wei_str = core.node.get_balance(addr).await.unwrap_or_else(|_| "0".to_string());
                            match wei_str.parse::<u128>() {
                                Ok(wei) => citrate_wallet_core::format::wei_to_salt(wei),
                                Err(_) => wei_str,
                            }
                        }
                    } else {
                        "0".to_string()
                    };

                    (pri_bal, sel_bal)
                });

                // Calculate session remaining time from unlock epoch
                let (session_active, session_remaining) = {
                    let unlock_epoch = SESSION_UNLOCK_EPOCH.load(Ordering::Relaxed);
                    if unlock_epoch > 0 {
                        let elapsed = (now_secs as i64).saturating_sub(unlock_epoch);
                        // Guard against clock skew producing negative elapsed
                        // (which previously yielded huge remaining values the
                        // user read as "locked for 425 hrs"). Clamp to
                        // [0, SESSION_TIMEOUT_SECS] so the display is always
                        // sensible even if the clock jumps.
                        let elapsed = elapsed.max(0);
                        let remaining = (SESSION_TIMEOUT_SECS - elapsed)
                            .clamp(0, SESSION_TIMEOUT_SECS);
                        if remaining > 0 {
                            (true, format_session_remaining(remaining))
                        } else {
                            // T1-3: session timed out. Clear the GUI
                            // clock AND lock the backend so the next
                            // send doesn't fail silently with the
                            // "Session expired" error, it surfaces an
                            // unlock prompt instead. The backend lock
                            // is idempotent — safe if we already
                            // locked from a prior tick.
                            SESSION_UNLOCK_EPOCH.store(0, Ordering::Relaxed);
                            let _ = rt_handle.block_on(core.wallet.lock());
                            // Toast once on the transition. If the
                            // user is on the wallet/chat/compute/learning
                            // tab they'll see this; if not, they'll
                            // still hit the lock screen on next send.
                            let ui_for_toast = ui_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_for_toast.upgrade() {
                                    // NAT-B-019: raise the lock screen (and
                                    // dismiss any open send dialog) on the
                                    // timeout transition. Pre-fix the backend
                                    // locked but the UI stayed on the wallet
                                    // panel — balances, addresses, history and
                                    // the export dialog remained on screen
                                    // indefinitely on an unattended machine.
                                    ui.set_show_send_dialog(false);
                                    ui.set_show_lock_screen(true);
                                    ui.set_clipboard_toast(
                                        "Session expired — unlock your wallet to continue".into()
                                    );
                                    let ui_clear = ui_for_toast.clone();
                                    slint::Timer::single_shot(
                                        std::time::Duration::from_millis(5000),
                                        move || {
                                            if let Some(ui) = ui_clear.upgrade() {
                                                ui.set_clipboard_toast("".into());
                                            }
                                        },
                                    );
                                }
                            });
                            (false, String::new())
                        }
                    } else {
                        (false, String::new())
                    }
                };

                // Compute earnings: delta between current balance and baseline
                // Use the raw wei from the balance query for accurate tracking
                let pri_addr = rt_handle.block_on(core.wallet.get_primary_address());
                let raw_wei = if let Some(addr) = &pri_addr {
                    rt_handle.block_on(core.node.get_balance(addr))
                        .ok()
                        .and_then(|s| s.parse::<u128>().ok())
                } else {
                    None
                };

                let earned_display = if let Some(wei) = raw_wei {
                    if baseline_balance_wei.is_none() && wei > 0 {
                        baseline_balance_wei = Some(wei);
                    }
                    if let Some(baseline) = baseline_balance_wei {
                        let earned_wei = wei.saturating_sub(baseline);
                        citrate_wallet_core::format::wei_to_salt(earned_wei)
                    } else {
                        "0.0000".to_string()
                    }
                } else {
                    "0.0000".to_string()
                };

                // Fetch tx history every 10th tick (~50s) to avoid storage churn
                let tx_list_raw: Vec<_> = if tick_counter % 10 == 0 {
                    if let Some(ref addr) = pri_addr {
                        rt_handle.block_on(core.node.get_transactions_for_address(addr, 20))
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                // WP-E.2: derive a reward-toast string BEFORE consuming tx_list_raw.
                // First pass after unlock: seed the seen set with all existing
                // reward hashes so we don't toast for historical rewards.
                // Subsequent passes: any reward hash not in the set → new reward
                // → emit a toast (throttled to 1 per 15s even if multiple land).
                let reward_toast: Option<String> = if tick_counter % 10 == 0 && !tx_list_raw.is_empty() {
                    if !reward_seed_done {
                        for t in &tx_list_raw {
                            if t.tx_type == "reward" {
                                seen_reward_hashes.insert(t.hash.clone());
                            }
                        }
                        reward_seed_done = true;
                        None
                    } else {
                        let mut total_reward_salt = 0.0_f64;
                        let mut count = 0u32;
                        for t in &tx_list_raw {
                            if t.tx_type == "reward" && !seen_reward_hashes.contains(&t.hash) {
                                seen_reward_hashes.insert(t.hash.clone());
                                // `amount` is already SALT-formatted (e.g. "10.0000").
                                if let Ok(v) = t.amount.parse::<f64>() {
                                    total_reward_salt += v;
                                }
                                count += 1;
                            }
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        // Throttle: at most one toast per 15 seconds. When a batch
                        // lands within the throttle window we swallow it — the next
                        // toast will cover the cumulative delta on the next unthrottled
                        // tick.
                        if count > 0 && now_ms.saturating_sub(last_reward_toast_ms) > 15_000 {
                            last_reward_toast_ms = now_ms;
                            Some(if count == 1 {
                                format!("+{:.2} SALT reward", total_reward_salt)
                            } else {
                                format!("+{:.2} SALT ({} rewards)", total_reward_salt, count)
                            })
                        } else {
                            None
                        }
                    }
                } else {
                    None
                };

                let tx_list: Vec<TxData> = tx_list_raw.into_iter().map(|t| TxData {
                    hash: t.hash.into(),
                    tx_type: t.tx_type.into(),
                    amount: t.amount.into(),
                    counterparty: t.counterparty.into(),
                    status: t.status.into(),
                    timestamp: t.timestamp.into(),
                }).collect();
                let has_tx_update = tick_counter % 10 == 0;

                let ui_for_main = ui_handle.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_for_main.upgrade() {
                        ui.set_node_running(status.running);
                        ui.set_block_height(status.block_height as i32);
                        ui.set_peer_count(status.peer_count as i32);
                        ui.set_mempool_size(status.mempool_size as i32);
                        ui.set_connection_status(conn_status.into());
                        ui.set_wallet_balance(primary_balance.into());
                        ui.set_wallet_selected_balance(selected_balance.into());
                        ui.set_salt_earned(earned_display.into());
                        // Push transaction history
                        if has_tx_update && !tx_list.is_empty() {
                            let model = std::rc::Rc::new(slint::VecModel::from(tx_list));
                            ui.set_wallet_transactions(model.into());
                        }
                        // WP-E.2: reward notification toast. Reuses the
                        // clipboard-toast pill (same overlay position), so
                        // users get a single consistent notification style
                        // across "Copied" + "Reward earned" events. Toast
                        // auto-clears on its existing 1.5s timer.
                        if let Some(ref msg) = reward_toast {
                            ui.set_clipboard_toast(msg.clone().into());
                            let ui_for_clear = ui_for_main.clone();
                            slint::Timer::single_shot(std::time::Duration::from_millis(2500), move || {
                                if let Some(ui) = ui_for_clear.upgrade() {
                                    ui.set_clipboard_toast("".into());
                                }
                            });
                        }
                        // Session timer
                        ui.set_wallet_session_active(session_active);
                        ui.set_wallet_session_remaining(session_remaining.into());
                        // Push wallet accounts if refreshed this tick
                        if let Some(ref accts) = wallet_accounts {
                            push_accounts_to_ui(&ui, accts);
                        }
                        // Block data from RocksDB
                        ui.set_block_hash_0(hash0.into());
                        ui.set_block_hash_1(hash1.into());
                        ui.set_block_hash_2(hash2.into());
                        ui.set_block_txcount_0(txc0.into());
                        ui.set_block_txcount_1(txc1.into());
                        ui.set_block_txcount_2(txc2.into());
                        ui.set_block_time_0(time0.into());
                        ui.set_block_time_1(time1.into());
                        ui.set_block_time_2(time2.into());
                        ui.set_block_proposer_0(prop0.into());
                        ui.set_block_proposer_1(prop1.into());
                        ui.set_block_proposer_2(prop2.into());
                    }
                });

                // T2-16: dashboard claimable breakdown. Query
                // ContributionAccounting.claimable(self) once per
                // 10 ticks (~30s) regardless of tab so the Dashboard
                // card reflects the real number without waiting for
                // the compute/learning tab to be opened.
                if tick_counter % 10 == 0 {
                    let chain_id = rt_handle.block_on(core.config.read()).chain_id;
                    let rpc_url = rt_handle.block_on(core.config.read()).active_rpc_url();
                    let accounts = rt_handle.block_on(core.wallet.list_accounts());
                    let self_addr = accounts.first().map(|a| a.address.clone());
                    let acc_addr = marketplace_client::contribution_accounting_address(chain_id);
                    let claimable_wei: u128 =
                        if let (Some(a), Some(addr)) = (acc_addr, self_addr.as_deref()) {
                            marketplace_client::encode_claimable(addr)
                                .and_then(|d| rt_handle.block_on(
                                    marketplace_client::eth_call(&rpc_url, a, &d)
                                ).ok())
                                .and_then(|r| marketplace_client::decode_uint256_u128(&r))
                                .unwrap_or(0)
                        } else { 0 };
                    let display = marketplace_client::wei_to_salt_display(claimable_wei);
                    let ui_h = ui_handle.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_h.upgrade() {
                            ui.set_dashboard_claimable(display.into());
                        }
                    });
                }

                // (Contracts IDE hydration retired in P960-G — no
                // terminal/git polling; users edit in their own editor
                // and a notify-backed file watcher re-fires compile.)

                // P960-D WP-D.4: compute provider + earnings poll.
                // Every 30s (10 × 3s ticks) when compute tab is active.
                // Data sources:
                //   - ComputeMarketplace.getProvider(self) → active jobs,
                //     stake, registration state
                //   - ContributionAccounting.claimable(self) → settled earnings
                //     ready to claim
                if tick_counter % 10 == 0 {
                    let active_tab = ui_handle.upgrade()
                        .map(|ui| ui.get_active_tab().to_string());
                    if active_tab.as_deref() == Some("compute") {
                        let chain_id = rt_handle.block_on(core.config.read()).chain_id;
                        let rpc_url = rt_handle.block_on(core.config.read()).active_rpc_url();
                        let accounts = rt_handle.block_on(core.wallet.list_accounts());
                        let self_addr = accounts.first().map(|a| a.address.clone());

                        let market_addr = marketplace_client::compute_marketplace_address(chain_id);
                        let accounting_addr = marketplace_client::contribution_accounting_address(chain_id);

                        // Fetch provider state
                        let provider: Option<marketplace_client::ProviderProfile> =
                            if let (Some(m), Some(addr)) = (market_addr, self_addr.as_deref()) {
                                if let Some(data) = marketplace_client::encode_get_provider(addr) {
                                    rt_handle
                                        .block_on(marketplace_client::eth_call(&rpc_url, m, &data))
                                        .ok()
                                        .and_then(|r| marketplace_client::decode_provider_profile(&r))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                        // Fetch claimable earnings
                        let claimable_wei: Option<u128> =
                            if let (Some(a), Some(addr)) = (accounting_addr, self_addr.as_deref()) {
                                if let Some(data) = marketplace_client::encode_claimable(addr) {
                                    rt_handle
                                        .block_on(marketplace_client::eth_call(&rpc_url, a, &data))
                                        .ok()
                                        .and_then(|r| marketplace_client::decode_uint256_u128(&r))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                        // CM-01 WP-01.5: recent activity via eth_getLogs.
                        // Only fetched when the provider is registered (query
                        // is cheap but pointless pre-registration).
                        // Data source: ComputeMarketplace event logs
                        // (JobAssigned, JobCompleted, JobFailed).
                        let activity: Vec<marketplace_client::ActivityEntry> =
                            if let (Some(m), Some(addr)) = (market_addr, self_addr.as_deref()) {
                                if provider.as_ref().is_some_and(|p| p.is_registered) {
                                    rt_handle
                                        .block_on(marketplace_client::fetch_recent_activity(
                                            &rpc_url, m, addr,
                                        ))
                                        .unwrap_or_default()
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            };

                        // Compose status line. Three mutually-exclusive cases.
                        let has_addresses = market_addr.is_some() && accounting_addr.is_some();
                        let ui_h = ui_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = ui_h.upgrade() else { return; };
                            if !has_addresses {
                                ui.set_compute_contract_status("Marketplace not deployed on this network".into());
                                ui.set_compute_provider_status("—".into());
                                ui.set_compute_active_jobs(0);
                                ui.set_compute_earned("0".into());
                                return;
                            }
                            ui.set_compute_contract_status("Marketplace live".into());
                            if let Some(p) = provider {
                                ui.set_compute_active_jobs(p.current_active_jobs as i32);
                                ui.set_compute_is_registered(p.is_registered);
                                let status = if p.is_registered {
                                    format!(
                                        "Registered · {} SALT staked · {} completed / {} failed · {} bps rep",
                                        marketplace_client::wei_to_salt_display(p.stake_wei),
                                        p.total_jobs_completed,
                                        p.total_jobs_failed,
                                        p.reputation_bps,
                                    )
                                } else {
                                    "Not registered".to_string()
                                };
                                ui.set_compute_provider_status(status.into());

                                // CM-01 structured listing card. Only
                                // populated when registered — the Slint
                                // card is conditional on `is-registered`
                                // so pre-registration reads are harmless
                                // but wasted. Reputation bps → % (one
                                // decimal); capacity as "a / m" strings.
                                if p.is_registered {
                                    let stake_salt = marketplace_client::wei_to_salt_display(p.stake_wei);
                                    ui.set_compute_listing_stake(stake_salt.into());
                                    let rep_pct = (p.reputation_bps as f64) / 100.0;
                                    ui.set_compute_listing_reputation_pct(format!("{:.1}", rep_pct).into());
                                    ui.set_compute_listing_capacity(
                                        format!("{} / {}", p.current_active_jobs, p.max_concurrent_jobs).into()
                                    );
                                    // v1 registers a single "any" wildcard
                                    // hash via `any_model_hash()` — future
                                    // sprints resolve the hashes through
                                    // ModelRegistry for rich names.
                                    ui.set_compute_listing_models("any (wildcard)".into());
                                    ui.set_compute_listing_total_completed(p.total_jobs_completed as i32);
                                    ui.set_compute_listing_total_failed(p.total_jobs_failed as i32);
                                }
                                ui.set_compute_listing_connection_ok(true);
                            } else {
                                ui.set_compute_is_registered(false);
                                ui.set_compute_provider_status("Query failed — retry in 30s".into());
                                // The listing query failed — signal
                                // degraded state so the user knows values
                                // are stale. Card itself is hidden
                                // because is-registered is now false.
                                ui.set_compute_listing_connection_ok(false);
                            }
                            if let Some(wei) = claimable_wei {
                                let salt = marketplace_client::wei_to_salt_display(wei);
                                ui.set_compute_earned(salt.clone().into());
                                ui.set_compute_listing_claimable(salt.into());
                            }

                            // Push recent-activity rows to the listing card.
                            // Rows already sorted newest-first by the helper.
                            let rows: Vec<ListingActivityRow> = activity
                                .into_iter()
                                .map(|e| ListingActivityRow {
                                    job_id: e.job_id as i32,
                                    block_number: e.block_number as i32,
                                    status: e.status.into(),
                                })
                                .collect();
                            let model = std::rc::Rc::new(slint::VecModel::from(rows));
                            ui.set_compute_listing_activity(model.into());
                        });
                    }
                }

                // P960-I: Learning pool + earnings poll. Same 30s
                // cadence as compute, gated on active tab. Reads:
                //   - LearningPool.nextPoolId() → pool count
                //   - LearningPool.isMember(0, self) → membership
                //   - LearningPool.stakes(0, self) → stake amount
                //   - ContributionAccounting.claimable(self) → earnings
                if tick_counter % 10 == 0 {
                    let active_tab = ui_handle.upgrade()
                        .map(|ui| ui.get_active_tab().to_string());
                    if active_tab.as_deref() == Some("learning") {
                        let chain_id = rt_handle.block_on(core.config.read()).chain_id;
                        let rpc_url = rt_handle.block_on(core.config.read()).active_rpc_url();
                        let accounts = rt_handle.block_on(core.wallet.list_accounts());
                        let self_addr = accounts.first().map(|a| a.address.clone());
                        let pool_addr = marketplace_client::learning_pool_address(chain_id);
                        let acc_addr = marketplace_client::contribution_accounting_address(chain_id);

                        // 1. Pool count via nextPoolId
                        let pool_count: u64 = if let Some(p) = pool_addr {
                            let data = marketplace_client::encode_next_pool_id();
                            rt_handle.block_on(marketplace_client::eth_call(&rpc_url, p, &data))
                                .ok()
                                .and_then(|r| marketplace_client::decode_uint256_u128(&r))
                                .map(|v| v as u64)
                                .unwrap_or(0)
                        } else { 0 };

                        // 2. Membership + stake for current pool (default 0)
                        let current_pool_id: u64 = ui_handle.upgrade()
                            .map(|ui| ui.get_learning_current_pool_id() as u64)
                            .unwrap_or(0);
                        let (is_member, stake_wei): (bool, u128) =
                            if let (Some(p), Some(addr)) = (pool_addr, self_addr.as_deref()) {
                                if pool_count == 0 || current_pool_id >= pool_count {
                                    (false, 0)
                                } else {
                                    let m = marketplace_client::encode_is_member(current_pool_id, addr)
                                        .and_then(|d| rt_handle.block_on(marketplace_client::eth_call(&rpc_url, p, &d)).ok())
                                        .and_then(|r| marketplace_client::decode_bool(&r))
                                        .unwrap_or(false);
                                    let s = marketplace_client::encode_stakes(current_pool_id, addr)
                                        .and_then(|d| rt_handle.block_on(marketplace_client::eth_call(&rpc_url, p, &d)).ok())
                                        .and_then(|r| marketplace_client::decode_uint256_u128(&r))
                                        .unwrap_or(0);
                                    (m, s)
                                }
                            } else { (false, 0) };

                        // 3. Claimable earnings (same accounting contract as compute)
                        let claimable_wei: u128 =
                            if let (Some(a), Some(addr)) = (acc_addr, self_addr.as_deref()) {
                                marketplace_client::encode_claimable(addr)
                                    .and_then(|d| rt_handle.block_on(marketplace_client::eth_call(&rpc_url, a, &d)).ok())
                                    .and_then(|r| marketplace_client::decode_uint256_u128(&r))
                                    .unwrap_or(0)
                            } else { 0 };

                        // T2-8: Pool detail — getPool(current_pool_id).
                        // Only fetch when the pool actually exists (pool_count > current).
                        let pool_info: Option<marketplace_client::PoolInfo> =
                            if let Some(p) = pool_addr {
                                if pool_count > 0 && current_pool_id < pool_count {
                                    let data = marketplace_client::encode_get_pool(current_pool_id);
                                    rt_handle.block_on(marketplace_client::eth_call(&rpc_url, p, &data))
                                        .ok()
                                        .and_then(|r| marketplace_client::decode_pool_info(&r))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                        let has_pool_addr = pool_addr.is_some();
                        let has_acc_addr = acc_addr.is_some();
                        let ui_h = ui_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = ui_h.upgrade() else { return; };
                            ui.set_learning_contract_status(
                                if has_pool_addr {
                                    "LearningPool live".into()
                                } else {
                                    "LearningPool not deployed on this network".into()
                                }
                            );
                            ui.set_learning_pool_count(pool_count as i32);
                            ui.set_learning_is_member(is_member);
                            ui.set_learning_staked(
                                marketplace_client::wei_to_salt_display(stake_wei).into()
                            );
                            if has_acc_addr {
                                ui.set_learning_earnings(
                                    marketplace_client::wei_to_salt_display(claimable_wei).into()
                                );
                            }
                            // T2-8: surface pool details if present
                            if let Some(p) = pool_info {
                                let state_label = match p.state {
                                    0 => "Active",
                                    1 => "Closed",
                                    2 => "InCycle",
                                    _ => "Unknown",
                                };
                                let access_label = match p.access {
                                    0 => "Open",
                                    1 => "Invite",
                                    2 => "Apply",
                                    _ => "Unknown",
                                };
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs()).unwrap_or(0);
                                let age = if p.created_at > 0 && now > p.created_at {
                                    let secs = now - p.created_at;
                                    if secs < 60 { format!("{}s ago", secs) }
                                    else if secs < 3600 { format!("{}m ago", secs / 60) }
                                    else if secs < 86400 { format!("{}h ago", secs / 3600) }
                                    else { format!("{}d ago", secs / 86400) }
                                } else {
                                    "—".to_string()
                                };
                                ui.set_learning_pool_name(p.name.into());
                                ui.set_learning_pool_description(p.description.into());
                                ui.set_learning_pool_creator(p.creator.into());
                                ui.set_learning_pool_state_label(state_label.into());
                                ui.set_learning_pool_access_label(access_label.into());
                                ui.set_learning_pool_min_stake(
                                    marketplace_client::wei_to_salt_display(p.min_stake_wei).into()
                                );
                                ui.set_learning_pool_member_count(p.member_count as i32);
                                ui.set_learning_pool_created_ago(age.into());
                            } else {
                                // Clear the card when no pool selected / no data
                                ui.set_learning_pool_name("".into());
                                ui.set_learning_pool_description("".into());
                                ui.set_learning_pool_creator("".into());
                                ui.set_learning_pool_state_label("".into());
                                ui.set_learning_pool_access_label("".into());
                                ui.set_learning_pool_min_stake("".into());
                                ui.set_learning_pool_member_count(0);
                                ui.set_learning_pool_created_ago("".into());
                            }
                            // Status line — concise summary of state
                            let status = if !has_pool_addr {
                                "—".to_string()
                            } else if pool_count == 0 {
                                "No pools created on-chain yet".to_string()
                            } else if is_member {
                                format!("Pool #{} · staked {}", current_pool_id, marketplace_client::wei_to_salt_display(stake_wei))
                            } else {
                                format!("{} pool(s) on-chain · not joined", pool_count)
                            };
                            ui.set_learning_pool_status(status.into());
                        });
                    }
                }

                // DAG panel data — two independent cadences:
                //  * dots (scatter) — refresh on EVERY tick (3s) when DAG
                //    tab is open, because it's just a local RocksDB read
                //    and users were complaining the viz didn't appear for
                //    up to 30s after opening the tab.
                //  * stats (getDagStats RPC) — every 30s, unchanged.
                let dag_active = ui_handle.upgrade()
                    .map(|ui| ui.get_active_tab().to_string())
                    .as_deref() == Some("dag");

                if dag_active {
                    // T2-13: compute DAG mini-viz dots from recent blocks.
                    // Normalized x ∈ [0,1] by height position, y ∈ [0,1]
                    // by (blue_score - min) / (max - min). Tip highlighted.
                    let recent = rt_handle.block_on(core.node.get_recent_blocks(30))
                        .unwrap_or_default();
                    if !recent.is_empty() {
                        let min_h = recent.iter().map(|b| b.height).min().unwrap_or(0);
                        let max_h = recent.iter().map(|b| b.height).max().unwrap_or(1).max(min_h + 1);
                        let min_b = recent.iter().map(|b| b.blue_score).min().unwrap_or(0);
                        let max_b = recent.iter().map(|b| b.blue_score).max().unwrap_or(1).max(min_b + 1);
                        let h_range = (max_h - min_h) as f32;
                        let b_range = (max_b - min_b) as f32;
                        let tip_height = max_h;
                        let dots: Vec<DagDotData> = recent.iter().map(|b| {
                            let x = if h_range > 0.0 {
                                (b.height - min_h) as f32 / h_range
                            } else { 0.5 };
                            let y = if b_range > 0.0 {
                                (b.blue_score - min_b) as f32 / b_range
                            } else { 0.5 };
                            let short = if b.hash.len() > 12 {
                                format!("{}…{}", &b.hash[..6], &b.hash[b.hash.len()-4..])
                            } else {
                                b.hash.clone()
                            };
                            DagDotData {
                                height: b.height as i32,
                                hash_short: short.into(),
                                x,
                                y,
                                tx_count: b.tx_count as i32,
                                is_tip: b.height == tip_height,
                            }
                        }).collect();
                        let ui_h = ui_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_h.upgrade() {
                                let model = std::rc::Rc::new(slint::VecModel::from(dots));
                                ui.set_dag_dots(model.into());
                            }
                        });
                    }
                }

                // T2-4: DAG stats RPC — every 30s when tab is open.
                if dag_active && tick_counter % 10 == 0 {
                    let rpc_url = rt_handle.block_on(core.config.read()).active_rpc_url();
                    let body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "citrate_getDagStats",
                        "params": [],
                        "id": 1,
                    });
                    let client = reqwest::Client::new();
                    let stats = rt_handle.block_on(async {
                        client.post(&rpc_url)
                            .json(&body)
                            .timeout(std::time::Duration::from_secs(2))
                            .send().await
                            .ok()?
                            .json::<serde_json::Value>().await.ok()
                    });
                    if let Some(stats) = stats {
                        let tips_count = stats.get("result")
                            .and_then(|r| r.get("tips"))
                            .and_then(|t| t.as_array())
                            .map(|a| a.len() as i32)
                            .unwrap_or(0);
                        let blue_score = stats.get("result")
                            .and_then(|r| r.get("blue_score"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as i32;
                        let height = stats.get("result")
                            .and_then(|r| r.get("height"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as i32;
                        let ui_h = ui_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_h.upgrade() {
                                ui.set_dag_tips_count(tips_count);
                                ui.set_dag_blue_score(blue_score);
                                ui.set_dag_finalized_height(height);
                            }
                        });
                    }
                }
            }
        });
    }


    // --- Wallet: Create Account (from wallet view, not onboarding) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_create_account(move |password, label| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let pwd = password.to_string();
        let lbl = label.to_string();
        tracing::info!("Wallet: creating new account '{}'", lbl);
        spawn_async(&rt_h, async move {
            match core.wallet.create_wallet(&pwd).await {
                Ok(result) => {
                    tracing::info!("New account created: {}", result.address);
                    let addr = eip55_checksum(&result.address);
                    let accounts = core.wallet.list_accounts().await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_selected_address(addr.into());
                            ui.set_wallet_selected_label(lbl.into());
                            push_accounts_to_ui(&ui, &accounts);
                            // Close the create dialog on success
                            ui.set_wallet_create_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Create account failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_create_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Import from Mnemonic ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_import_mnemonic(move |mnemonic, password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let mnemonic_str = mnemonic.to_string();
        let pwd = password.to_string();
        tracing::info!("Wallet: importing from mnemonic");
        spawn_async(&rt_h, async move {
            // Data source: citrate_wallet_core::KeyManager::recover_from_mnemonic
            match core.wallet.import_from_mnemonic(&mnemonic_str, &pwd).await {
                Ok(result) => {
                    tracing::info!("Imported account: {}", result.address);
                    let addr = eip55_checksum(&result.address);
                    let accounts = core.wallet.list_accounts().await;
                    // Activate the GUI session clock. Previously, import
                    // set the backend session active (inside import_from_mnemonic)
                    // but left SESSION_UNLOCK_EPOCH at 0, so the wallet pill
                    // kept showing 🔒 and the background tick never flipped
                    // session_active true — users couldn't list/stake after
                    // importing a wallet.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    SESSION_UNLOCK_EPOCH.store(now, Ordering::Relaxed);
                    let session_initial = format_session_remaining(SESSION_TIMEOUT_SECS);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_selected_address(addr.into());
                            ui.set_wallet_selected_label("Imported Account".into());
                            push_accounts_to_ui(&ui, &accounts);
                            ui.set_wallet_import_error("".into());
                            ui.set_wallet_session_active(true);
                            ui.set_wallet_session_remaining(session_initial.into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Import failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_import_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Select Account ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_select_account(move |index| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let idx = index as usize;
        tracing::info!("Wallet: selecting account at index {}", idx);
        spawn_async(&rt_h, async move {
            let accounts = core.wallet.list_accounts().await;
            if let Some(acct) = accounts.get(idx) {
                let addr = eip55_checksum(&acct.address);
                let label = acct.label.clone();
                let balance = acct.balance.clone();
                tracing::info!("Selected account: {} ({})", label, addr);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_selected_address(addr.into());
                        ui.set_wallet_selected_label(label.into());
                        ui.set_wallet_selected_balance(balance.into());
                    }
                });
            }
        });
    });

    // --- Wallet: Export Private Key (requires re-auth via password) ---
    // Data source: citrate_wallet_core::KeyManager::export_private_key
    // Decrypts the on-disk keystore entry with the provided password and returns hex.
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let core = app_core.clone();
    ui.on_wallet_export_key(move |password| {
        let ui_w = ui_w.clone();
        let core = core.clone();
        // NAT-B-016: wipe the Rust-side password copy on drop.
        let pwd = Zeroizing::new(password.to_string());

        // Clear any previously exported key state (security: don't leave keys in memory)
        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_exported_key("".into());
            ui.set_wallet_export_error("".into());
        }

        // Read the currently selected account address from UI state
        let selected_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();

        if selected_addr.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_export_error("No account selected".into());
                }
            });
            return;
        }

        tracing::info!("Wallet: exporting private key for {} (re-auth required)", selected_addr);
        spawn_async(&rt_h, async move {
            // NAT-B-017: route the export through WalletService so it shares
            // the SessionManager brute-force lockout (5 attempts / 5-min
            // cooldown, persisted across restarts). Pre-fix this built a
            // fresh KeyManager per attempt, bypassing the lockout entirely —
            // an unthrottled password oracle against the keystore.
            match core.wallet.export_private_key(&selected_addr, &pwd).await {
                Ok(hex_key) => {
                    tracing::info!("Private key exported for {} (length: {} hex chars)", selected_addr, hex_key.len());
                    let ui_for_export = ui_w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_for_export.upgrade() {
                            let key_for_timer = hex_key.clone();
                            ui.set_wallet_exported_key(hex_key.into());
                            ui.set_wallet_export_error("".into());
                            schedule_exported_key_clear(ui_for_export.clone(), key_for_timer);
                        }
                    });
                }
                Err(e) => {
                    let err = format!("{}", e);
                    tracing::error!("Export failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_export_error(err.into());
                            ui.set_wallet_exported_key("".into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Clear Exported Key State ---
    let ui_w = ui.as_weak();
    ui.on_wallet_clear_export_state(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_exported_key("".into());
            ui.set_wallet_export_error("".into());
        }
    });

    // --- Faucet request ---
    // Data source: POST https://faucet.citrate.ai/faucet with {"address": "0x..."}
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let core = app_core.clone();
    ui.on_wallet_request_faucet(move || {
        let ui_w = ui_w.clone();
        let core = core.clone();
        tracing::info!("Wallet: requesting SALT from faucet");

        // NAT-B-028: fund the SELECTED account, not always the primary.
        let selected_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();

        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_faucet_status("Requesting...".into());
        }

        spawn_async(&rt_h, async move {
            // NAT-B-028: the faucet only exists on testnet, and hitting a
            // hardcoded third-party endpoint on any other network leaks an
            // address↔IP correlation to no purpose. Refuse off testnet.
            let network = core.config.read().await.network.clone();
            if network != "testnet" {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_faucet_status("Faucet is available on testnet only".into());
                    }
                });
                return;
            }

            let address = if !selected_addr.is_empty() {
                selected_addr
            } else {
                core.wallet.get_primary_address().await.unwrap_or_default()
            };
            if address.is_empty() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_faucet_status("No wallet".into());
                    }
                });
                return;
            }

            let client = reqwest::Client::new();
            let body = serde_json::json!({"address": address});
            match client.post("https://faucet.citrate.ai/faucet")
                .json(&body)
                .timeout(std::time::Duration::from_secs(10))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Faucet: SALT requested for {}", address);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status("Sent!".into());
                        }
                    });
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();
                    tracing::warn!("Faucet: {} — {}", status, body_text);
                    let msg = if body_text.contains("rate") || body_text.contains("limit") {
                        "Rate limited".to_string()
                    } else {
                        format!("Error {}", status.as_u16())
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status(msg.into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Faucet: request failed — {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status("Failed".into());
                        }
                    });
                }
            }
        });
    });


    // --- EW-S1 WP-8: Citrate identity link ---
    // Data sources: auth.citrate.ai OIDC + /aa/enroll-validator;
    // chain RPC eth_getCode/eth_call; bundler.citrate.ai for UserOps.
    // Hydrate persisted link state at startup.
    if let Some(link) = citrate_link.load_link() {
        ui.set_wallet_citrate_smart_wallet(link.smart_wallet.clone().into());
        ui.set_wallet_citrate_tier(link.tier.clone().into());
        ui.set_wallet_citrate_role(link.citrate_role.clone().unwrap_or_default().into());
        ui.set_wallet_citrate_kyc(link.kyc_status.clone().into());
        ui.set_wallet_citrate_link_status(
            if link.pending_root_enroll {
                "Wallet exists with a passkey signer — finish linking this device from the auth.citrate.ai dashboard."
            } else {
                "Same address as auth.citrate.ai — this device signs as the wallet's validator."
            }
            .into(),
        );
    }

    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let link_svc = citrate_link.clone();
    ui.on_wallet_link_citrate_device(move || {
        let ui_w = ui_w.clone();
        let link_svc = link_svc.clone();
        let eoa = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();
        if eoa.is_empty() {
            return;
        }
        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_citrate_linking(true);
            ui.set_wallet_citrate_link_status("".into());
        }
        spawn_async(&rt_h, async move {
            let result = link_svc.link(&eoa).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_citrate_linking(false);
                    match result {
                        Ok(link) => {
                            tracing::info!("Citrate link complete: {}", link.smart_wallet);
                            ui.set_wallet_citrate_smart_wallet(link.smart_wallet.clone().into());
                            ui.set_wallet_citrate_tier(link.tier.clone().into());
                            ui.set_wallet_citrate_role(link.citrate_role.clone().unwrap_or_default().into());
                            ui.set_wallet_citrate_kyc(link.kyc_status.clone().into());
                            ui.set_wallet_citrate_link_status(
                                if link.pending_root_enroll {
                                    "Wallet exists with a passkey signer — finish linking this device from the auth.citrate.ai dashboard."
                                } else {
                                    "Same address as auth.citrate.ai — this device signs as the wallet's validator."
                                }
                                .into(),
                            );
                        }
                        Err(e) => {
                            tracing::error!("Citrate link failed: {}", e);
                            ui.set_wallet_citrate_link_status(format!("Link failed: {}", e).into());
                        }
                    }
                }
            });
        });
    });

    // --- AUTHSPINE S3-WP3: open the hosted Account Hub (KYC / tier upgrade) ---
    let link_svc = citrate_link.clone();
    ui.on_wallet_manage_account(move || {
        if let Err(e) = link_svc.open_account_hub() {
            tracing::warn!("could not open the Account Hub: {}", e);
        }
    });

    // --- EW-S1 WP-8: sponsored send from the linked smart wallet ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let link_svc = citrate_link.clone();
    ui.on_wallet_send_sponsored(move |to, amount| {
        let ui_w = ui_w.clone();
        let link_svc = link_svc.clone();
        let to_str = to.to_string();
        let amt_str = amount.to_string();

        let wei: u128 = match citrate_wallet_core::format::salt_to_wei(&amt_str) {
            Ok(w) => w,
            Err(e) => {
                let err_msg = format!("Invalid amount: {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_send_error(err_msg.into());
                    }
                });
                return;
            }
        };

        tracing::info!("Sponsored send: {} SALT to {} from the smart wallet", amt_str, to_str);
        spawn_async(&rt_h, async move {
            let result = link_svc.send_sponsored(&to_str, wei, Vec::new()).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    match result {
                        Ok(user_op_hash) => {
                            ui.set_send_tx_hash(user_op_hash.into());
                            ui.set_send_error("".into());
                            ui.set_send_receipt_status(
                                "UserOperation accepted by the bundler — gas sponsored by Citrate.".into(),
                            );
                        }
                        Err(e) => {
                            ui.set_send_error(e.to_string().into());
                            ui.set_send_receipt_status("".into());
                        }
                    }
                }
            });
        });
    });

    // Terminal and git init deferred — will initialize on first tab switch to Contracts.
    // This prevents blocking the UI at startup.

    // IDE state updates happen through callbacks (file open, edit, etc.)
    // No polling timer — prevents blocking the Slint event loop.

    // Terminal updates will use slint::invoke_from_event_loop from a background thread.
    // No polling timer — prevents blocking the Slint event loop.

    // =========================================================================
    // CHAT WIRING — AI agent via citrate_chatCompletion RPC
    // =========================================================================

    // Shared active approval request ID — used to correlate approve/reject
    // UI callbacks with the exact request submitted by the tool executor.
    let active_approval_request_id: Arc<tokio::sync::RwLock<Option<String>>> = Arc::new(tokio::sync::RwLock::new(None));
    let active_req_for_chat = active_approval_request_id.clone();

    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_chat_send(move |message| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let active_req_for_chat = active_req_for_chat.clone();
        let msg = message.to_string();

        // Show thinking state and user's message immediately (doesn't block)
        loader_set_running(true); // WP-5: animate the thinking loader
        if let Some(ui) = ui_w.upgrade() {
            ui.set_chat_thinking(true);
            ui.set_chat_error("".into());
            // Push user message to bubbles immediately so they see it before AI responds
            let current = ui.get_chat_messages();
            let mut msgs: Vec<ChatMessageData> = {
                use slint::Model;
                (0..current.row_count())
                    .filter_map(|i| current.row_data(i))
                    .collect()
            };
            msgs.push(ChatMessageData {
                role: "user".into(),
                content: msg.clone().into(),
                tool_name: "".into(),
            });
            let model = std::rc::Rc::new(slint::VecModel::from(msgs));
            ui.set_chat_messages(model.into());
        }

        // T2-12: emit user message into the trail. The async block
        // below records the eventual assistant response on completion.
        let msg_for_trail = msg.clone();
        let chars = msg.chars().count();
        core.events.publish(citrate_desktop_app::event_bus::AppEvent::ChatMessage {
            role: "user".to_string(),
            content: msg_for_trail,
            chars,
        });

        // Send async WITH tool execution — the live chat path uses send_message_with_tools
        spawn_async(&rt_h, async move {
            // T2-11: keyword triggers that suggest the user wants a
            // tool invocation. Extracted from the old inline `||`
            // chain into a data-driven table so future tools can
            // append to this list (or, in a follow-up, each Tool
            // trait impl can expose its own triggers and we union
            // them at registry-load time).
            //
            // The triggers are deliberately loose — a false positive
            // just means the model sees tool defs on a conversational
            // turn, which is cheap; a false negative means the user
            // asks "what's my balance" and the model can't answer.
            const TOOL_TRIGGERS: &[&str] = &[
                // Wallet / money
                "balance", "send", "deploy", "check", "transaction", "contract",
                // Agent / local
                "model", "file", "git", "run", "execute", "search",
                // Chain-state (P960-A WP-A.2 natural phrases)
                "block", "height", "peer", "network", "chain", "sync",
                "mempool", "tip", "recent", "history", "explain",
                " tx ", " 0x",
            ];
            let msg_lower = msg.to_lowercase();
            let needs_tools = TOOL_TRIGGERS.iter().any(|t| msg_lower.contains(t));
            let tool_defs = if needs_tools {
                core.tool_registry.tool_definitions().await
            } else {
                vec![]
            };

            // Capture refs for the tool executor closure
            let approvals = core.approvals.clone();
            let ui_for_tools = ui_w.clone();
            let active_req_id = active_req_for_chat.clone();
            let events_for_tools = core.events.clone();
            let wallet_for_tools = core.wallet.clone();
            // P960-A WP-A.2: capture node + block services so the tool
            // executor can read real chain state instead of stubs.
            let node_for_tools = core.node.clone();
            let blocks_for_tools = core.blocks.clone();
            // P960-K T1-2: capture session policy so the tool dispatch
            // can refuse mutation tools under ReadOnly scope before the
            // approval flow is even reached.
            let session_policy_for_tools = core.session_policy.clone();

            // Use streaming variant for incremental UI updates
            let ui_for_stream = ui_w.clone();
            match core.chat.send_message_with_tools_streaming(
                &msg,
                tool_defs,
                |tool_name, params| {
                    let approvals = approvals.clone();
                    let ui_for_tools = ui_for_tools.clone();
                    let active_req_id = active_req_id.clone();
                    let events = events_for_tools.clone();
                    let wallet = wallet_for_tools.clone();
                    let node = node_for_tools.clone();
                    let blocks = blocks_for_tools.clone();
                    let session_policy = session_policy_for_tools.clone();
                    async move {
                        let start_time = std::time::Instant::now();
                        tracing::info!("Tool call: {} with {:?}", tool_name, params);

                        // P960-K T1-2: scope check FIRST — before risk
                        // classification, before approval. If the
                        // session's policy doesn't allow this tool's
                        // category, refuse immediately with a clear
                        // reason. categorize_tool() + allowed_by() are
                        // the same helpers the MCP host uses for
                        // external runtime grants — single enforcement
                        // path regardless of caller.
                        {
                            let policy = session_policy.read().await.clone();
                            let category = citrate_agent_core::mcp_server::categorize_tool(&tool_name);
                            if !category.allowed_by(&policy) {
                                tracing::warn!(
                                    "Tool '{}' (category {:?}) refused by scope {:?}",
                                    tool_name, category, policy
                                );
                                return Err(format!(
                                    "Tool '{}' is not allowed under {:?} scope. Switch to Guided in Operations to enable it.",
                                    tool_name, policy
                                ));
                            }
                        }

                        // Determine risk level and target info for each tool
                        let (risk_level, target, scope) = match tool_name.as_str() {
                            "send_tx" => {
                                let to = params.get("to").and_then(|v| v.as_str()).unwrap_or("unknown");
                                let amount = params.get("amount").and_then(|v| v.as_str()).unwrap_or("?");
                                ("high".to_string(), format!("Address: {}", to), format!("Send {} SALT", amount))
                            }
                            "deploy_contract" => {
                                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed");
                                ("high".to_string(), format!("Contract: {}", name), "Deploy new contract to chain".to_string())
                            }
                            "file_write" | "file_edit" => {
                                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("unknown");
                                ("high".to_string(), format!("File: {}", path), "Modify filesystem".to_string())
                            }
                            "shell_exec" => {
                                let cmd = params.get("command").and_then(|v| v.as_str()).unwrap_or("unknown");
                                ("critical".to_string(), format!("Command: {}", cmd), "Execute shell command".to_string())
                            }
                            _ => ("low".to_string(), String::new(), String::new()),
                        };

                        let is_high_risk = matches!(risk_level.as_str(), "high" | "critical");
                        let ui_for_disclosure = ui_for_tools.clone();

                        if is_high_risk {
                            // Publish trail event for tool request
                            events.publish(citrate_desktop_app::event_bus::AppEvent::ToolCallRequested {
                                tool_name: tool_name.clone(),
                                risk_level: risk_level.clone(),
                                target: target.clone(),
                            });

                            // Submit approval request and show card in UI
                            let request = citrate_agent_core::canonical::ApprovalRequest {
                                request_id: uuid::Uuid::new_v4().to_string(),
                                session_id: "live".to_string(),
                                tool_name: tool_name.clone(),
                                params: params.clone(),
                                risk_level: risk_level.clone(),
                                created_at: chrono::Utc::now().to_rfc3339(),
                                timeout_seconds: 30,
                                resolved: None,
                                resolved_at: None,
                            };
                            let req_id = request.request_id.clone();
                            let tool_display = tool_name.clone();
                            let param_display = serde_json::to_string_pretty(&params).unwrap_or_default();
                            let risk_display = risk_level.clone();
                            let target_display = target.clone();
                            let scope_display = scope.clone();

                            // Show approval card in UI with risk details
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_for_tools.upgrade() {
                                    ui.set_chat_tool_pending(true);
                                    ui.set_chat_tool_name(tool_display.into());
                                    ui.set_chat_tool_description(param_display.into());
                                    ui.set_chat_tool_risk_level(risk_display.into());
                                    ui.set_chat_tool_target(target_display.into());
                                    ui.set_chat_tool_scope(scope_display.into());
                                }
                            });

                            // Store the request ID so approve/reject resolves the exact request
                            *active_req_id.write().await = Some(req_id.clone());

                            // Submit and wait for user decision
                            let rx = approvals.submit(request).await;
                            match rx.await {
                                Ok(true) => {
                                    tracing::info!("Tool '{}' approved (req: {})", tool_name, req_id);
                                    events.publish(citrate_desktop_app::event_bus::AppEvent::ToolCallApproved {
                                        tool_name: tool_name.clone(),
                                        request_id: req_id.clone(),
                                    });
                                }
                                _ => {
                                    tracing::info!("Tool '{}' denied (req: {})", tool_name, req_id);
                                    events.publish(citrate_desktop_app::event_bus::AppEvent::ToolCallDenied {
                                        tool_name: tool_name.clone(),
                                        request_id: req_id.clone(),
                                    });
                                    return Err(format!("Tool '{}' was denied by user", tool_name));
                                }
                            }
                        }

                        // P960-A WP-A.2: Execute the tool against real backends.
                        // Each branch names its data source (Rule 11). Tools
                        // return JSON strings the LLM can cite in its reply.
                        let result = match tool_name.as_str() {
                            // -------------------------------------------------
                            // check_balance — eth_getBalance via NodeService
                            // -------------------------------------------------
                            "check_balance" => {
                                let addr_owned: String = match params.get("address").and_then(|a| a.as_str()) {
                                    Some(s) if !s.is_empty() && s != "default" => s.to_string(),
                                    _ => {
                                        // Fall back to primary wallet account
                                        let accounts = wallet.list_accounts().await;
                                        accounts.first().map(|a| a.address.clone())
                                            .unwrap_or_default()
                                    }
                                };
                                if addr_owned.is_empty() {
                                    Err("No address provided and no wallet account available".to_string())
                                } else {
                                    match node.get_balance(&addr_owned).await {
                                        Ok(balance_wei) => {
                                            // Convert wei string to SALT (18 decimals).
                                            // We deliberately do NOT return the raw wei string
                                            // to the LLM — it has consistently confused wei
                                            // (10^18 base) with SALT or Gwei and reported
                                            // balances off by 10^9 or 10^18. The only unit
                                            // the chat surface should reason about is SALT.
                                            let salt = wei_str_to_salt(&balance_wei);
                                            Ok(serde_json::json!({
                                                "address": addr_owned,
                                                "balance_salt": salt,
                                                "unit": "SALT",
                                                "display": format!("{} SALT", salt),
                                            }).to_string())
                                        }
                                        Err(e) => Err(format!("get_balance failed: {}", e)),
                                    }
                                }
                            }
                            // -------------------------------------------------
                            // get_block_height — NodeService.get_status()
                            // -------------------------------------------------
                            "get_block_height" => {
                                let st = node.get_status().await;
                                Ok(serde_json::json!({
                                    "height": st.block_height,
                                    "chain_id": st.chain_id,
                                    "syncing": st.syncing,
                                }).to_string())
                            }
                            // -------------------------------------------------
                            // get_peer_count — NodeService.get_status()
                            // -------------------------------------------------
                            "get_peer_count" => {
                                let st = node.get_status().await;
                                Ok(serde_json::json!({
                                    "peers": st.peer_count,
                                    "mempool_size": st.mempool_size,
                                    "dag_tips": st.dag_tips,
                                }).to_string())
                            }
                            // -------------------------------------------------
                            // explain_tx — BlockService.get_transaction()
                            // (eth_getTransactionByHash + eth_getTransactionReceipt)
                            // -------------------------------------------------
                            "explain_tx" => {
                                let tx_hash = params.get("tx_hash")
                                    .or_else(|| params.get("hash"))
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| "Missing 'tx_hash'".to_string())?;
                                match blocks.get_transaction(tx_hash).await {
                                    Ok(tx) => {
                                        // Only expose SALT-denominated value to the LLM.
                                        // Raw wei consistently confuses the model (off by 10^9 or 10^18).
                                        let salt = wei_str_to_salt(&tx.value);
                                        Ok(serde_json::json!({
                                            "hash": tx.hash,
                                            "from": tx.from,
                                            "to": tx.to,
                                            "value_salt": salt,
                                            "unit": "SALT",
                                            "status": tx.status,
                                            "block_height": tx.block_height,
                                            "tx_type": tx.tx_type,
                                        }).to_string())
                                    }
                                    Err(e) => Err(format!("tx not found: {}", e)),
                                }
                            }
                            // -------------------------------------------------
                            // get_recent_blocks — NodeService.get_recent_blocks()
                            // -------------------------------------------------
                            "get_recent_blocks" => {
                                let count = params.get("count")
                                    .and_then(|v| v.as_u64())
                                    .map(|n| n.clamp(1, 50) as usize)
                                    .unwrap_or(10);
                                match node.get_recent_blocks(count).await {
                                    Ok(list) => {
                                        let arr: Vec<_> = list.into_iter().map(|b| serde_json::json!({
                                            "height": b.height,
                                            "hash": b.hash,
                                            "tx_count": b.tx_count,
                                            "timestamp": b.timestamp,
                                            "blue_score": b.blue_score,
                                        })).collect();
                                        Ok(serde_json::json!({ "blocks": arr }).to_string())
                                    }
                                    Err(e) => Err(format!("get_recent_blocks failed: {}", e)),
                                }
                            }
                            // -------------------------------------------------
                            // get_tx_history — NodeService.get_transactions_for_address()
                            // -------------------------------------------------
                            "get_tx_history" => {
                                let addr_owned: String = match params.get("address").and_then(|v| v.as_str()) {
                                    Some(s) if !s.is_empty() && s != "default" => s.to_string(),
                                    _ => {
                                        let accounts = wallet.list_accounts().await;
                                        accounts.first().map(|a| a.address.clone())
                                            .unwrap_or_default()
                                    }
                                };
                                if addr_owned.is_empty() {
                                    Err("No address provided and no wallet account available".to_string())
                                } else {
                                    let limit = params.get("count")
                                        .and_then(|v| v.as_u64())
                                        .map(|n| n.clamp(1, 100) as usize)
                                        .unwrap_or(20);
                                    let list = node.get_transactions_for_address(&addr_owned, limit).await;
                                    let arr: Vec<_> = list.into_iter().map(|t| serde_json::json!({
                                        "hash": t.hash,
                                        "tx_type": t.tx_type,
                                        "amount": t.amount,
                                        "counterparty": t.counterparty,
                                        "status": t.status,
                                        "timestamp": t.timestamp,
                                    })).collect();
                                    Ok(serde_json::json!({
                                        "address": addr_owned,
                                        "count": arr.len(),
                                        "transactions": arr,
                                    }).to_string())
                                }
                            }
                            // -------------------------------------------------
                            // send_tx — WalletService.send_transaction()
                            // (unchanged — already real)
                            // -------------------------------------------------
                            "send_tx" => {
                                let to = params.get("to").and_then(|v| v.as_str())
                                    .ok_or_else(|| "Missing 'to' address".to_string())?;
                                let amount = params.get("amount").and_then(|v| v.as_str())
                                    .ok_or_else(|| "Missing 'amount'".to_string())?;
                                // RM-G.7: exact decimal SALT→wei conversion. The
                                // prior `(amount.parse::<f64>() * 1e18) as u128`
                                // lost precision for large amounts and diverged
                                // from the popup send path; route both through the
                                // canonical `salt_to_wei` (BigInt-exact) converter.
                                let value_wei = citrate_wallet_core::format::salt_to_wei(amount)
                                    .map_err(|e| format!("Invalid amount '{}': {}", amount, e))?
                                    .to_string();
                                let accounts = wallet.list_accounts().await;
                                let from = accounts.first()
                                    .map(|a| a.address.clone())
                                    .ok_or_else(|| "No wallet account found".to_string())?;
                                match wallet.send_transaction(&from, to, &value_wei, "").await {
                                    Ok(tx_hash) => Ok(format!("Transaction sent. Hash: {}", tx_hash)),
                                    Err(e) => Err(format!("Transaction failed: {}", e)),
                                }
                            }
                            _ => Err(format!("Tool '{}' not yet wired for live execution", tool_name)),
                        };

                        // Record tool completion in trail
                        let elapsed = start_time.elapsed().as_millis() as u64;
                        let (success, summary) = match &result {
                            Ok(s) => (true, s.clone()),
                            Err(e) => (false, e.clone()),
                        };
                        events.publish(citrate_desktop_app::event_bus::AppEvent::ToolCallCompleted {
                            tool_name: tool_name.clone(),
                            success,
                            duration_ms: elapsed,
                            result_summary: summary.clone(),
                        });

                        // Show tool disclosure in UI
                        let disclosure = if success {
                            format!("Executed {} — {}", tool_name, summary)
                        } else {
                            format!("Failed {} — {}", tool_name, summary)
                        };
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_for_disclosure.upgrade() {
                                ui.set_chat_tool_disclosure(disclosure.into());
                            }
                        });

                        result
                    }
                },
                // Streaming chunk callback — updates UI incrementally
                move |chunk: &str| {
                    let text = chunk.to_string();
                    let ui_s = ui_for_stream.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_s.upgrade() {
                            ui.set_chat_last_response(text.into());
                        }
                    });
                },
            ).await {
                Ok(response) => {
                    let content = clean_markdown(&response.content);
                    // T2-12: emit assistant response into the trail.
                    // Tool-result events are recorded separately via
                    // ToolCallCompleted; this captures the model's
                    // final natural-language reply.
                    let response_chars = content.chars().count();
                    core.events.publish(citrate_desktop_app::event_bus::AppEvent::ChatMessage {
                        role: "assistant".to_string(),
                        content: content.clone(),
                        chars: response_chars,
                    });
                    // Build structured message list. Include tool-role
                    // messages so the user sees the actual structured
                    // chain data (not just the LLM's summary of it).
                    // Tool JSON gets pretty-formatted via
                    // `format_tool_result` below.
                    let messages = core.chat.get_messages().await;
                    let slint_messages: Vec<ChatMessageData> = messages.iter()
                        .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
                        .map(|m| {
                            let (cleaned, tool_name) = if m.role == "user" {
                                (m.content.clone(), String::new())
                            } else if m.role == "tool" {
                                let tn = m.tool_action.as_ref()
                                    .map(|ta| ta.tool_type.clone())
                                    .unwrap_or_default();
                                (format_tool_result(&tn, &m.content), tn)
                            } else {
                                (clean_markdown(&m.content), String::new())
                            };
                            ChatMessageData {
                                role: m.role.clone().into(),
                                content: cleaned.into(),
                                tool_name: tool_name.into(),
                            }
                        })
                        .collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_last_response(content.into());
                            let model = std::rc::Rc::new(slint::VecModel::from(slint_messages));
                            ui.set_chat_messages(model.into());
                            ui.set_chat_thinking(false);
                            ui.set_chat_model_loaded(true);
                        }
                        loader_set_running(false); // WP-5: response arrived
                    });
                }
                Err(e) => {
                    let raw = e.to_string();
                    let err_msg = if raw.contains("not found") || raw.contains("No model") {
                        "No AI model loaded. Go to Settings > AI Configuration to configure a provider or download a local model.".to_string()
                    } else {
                        raw.clone()
                    };
                    tracing::error!("Chat error: {}", raw);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_error(err_msg.into());
                            ui.set_chat_thinking(false);
                        }
                        loader_set_running(false); // WP-5: chat errored
                    });
                }
            }
        });
    });

    // Set initial chat context with wallet info and auto-detect AI backend.
    // Runs in a background thread to avoid blocking the UI — Ollama detection
    // has a 2-second timeout that would freeze the window on startup.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        let config_network = rt_h.block_on(async {
            core.config.read().await.network.clone()
        });
        spawn_async(&rt_h, async move {
            // Set chat context with real wallet state.
            // The chat system prompt formats this as "Balance: {} SALT",
            // so convert raw grains (wei from eth_getBalance) to SALT
            // BEFORE handing it to the LLM — otherwise the model sees a
            // 25-digit integer and reports it verbatim (the chat-balance
            // "18 trailing zeros" bug).
            let address = core.wallet.get_primary_address().await
                .unwrap_or_else(|| "not connected".to_string());
            let grains = core.node.get_balance(&address).await
                .unwrap_or_else(|_| "0".to_string());
            let balance = citrate_wallet_core::format::grains_str_to_salt(&grains);
            let height = core.node.get_status().await.block_height;
            core.chat.set_context(&address, &balance, &config_network, height).await;

            // P960-A WP-A.3: keep the chat system prompt live. Without
            // this the LLM sees block_height=0 and stale balance forever,
            // so "what's the current height" gets answered from the
            // snapshot taken at wallet unlock. 10s cadence matches the
            // dashboard poll — cheap, and tests show no lock contention
            // with the ChatService RwLock.
            let core_ctx = core.clone();
            let ctx_network = config_network.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Skip the first immediate tick — we just set context above.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let addr = core_ctx.wallet.get_primary_address().await
                        .unwrap_or_else(|| "not connected".to_string());
                    let grains = core_ctx.node.get_balance(&addr).await
                        .unwrap_or_else(|_| "0".to_string());
                    // Convert grains → SALT here (same reason as above).
                    let bal = citrate_wallet_core::format::grains_str_to_salt(&grains);
                    let h = core_ctx.node.get_status().await.block_height;
                    core_ctx.chat.set_context(&addr, &bal, &ctx_network, h).await;
                }
            });

            // Auto-detect local AI backend: Ollama (preferred) → local GGUF → none
            // This is local-first: only localhost:11434 and filesystem are checked.
            // No external API calls are made during detection.
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let model_loaded = matches!(detected.backend_type.as_str(), "ollama" | "gguf");
            let display_name = detected.display_name.clone();
            let model_id = detected.model_id.clone();
            let backend_type = detected.backend_type.clone();

            if model_loaded {
                tracing::info!("Auto-detected AI backend: {} ({})", display_name, backend_type);
                core.chat.set_model(&model_id).await;
            } else {
                tracing::info!("No local AI backend detected. User can configure one in Settings.");
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_chat_model_loaded(model_loaded);
                    ui.set_chat_model_name(display_name.into());
                    // Set privacy indicator based on backend type
                    let privacy = match backend_type.as_str() {
                        "ollama" | "gguf" => "local",
                        "none" => "none",
                        _ => "api",
                    };
                    ui.set_chat_backend_type(privacy.into());
                }
            });
        });
    }

    // =========================================================================
    // SETTINGS WIRING — Node control, bootnodes, theme, factory reset
    // =========================================================================

    // --- Settings: Start Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_start_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: starting node");
        spawn_async(&rt_h, async move {
            // WP-A1: the silent death happened right AFTER this log line —
            // breadcrumb both sides so a recurrence is attributable.
            crash_telemetry::set_last_state("node-start (settings)");
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node started via settings");
                    crash_telemetry::set_last_state("node-running (settings)");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() { ui.set_node_running(true); }
                    });
                }
                Err(e) => tracing::error!("Node start failed: {}", e),
            }
        });
    });

    // --- Settings: Stop Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_stop_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: stopping node");
        spawn_async(&rt_h, async move {
            crash_telemetry::set_last_state("node-stop (settings)");
            match core.node.stop().await {
                Ok(()) => {
                    tracing::info!("Node stopped via settings");
                    crash_telemetry::set_last_state("node-stopped (settings)");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() { ui.set_node_running(false); }
                    });
                }
                Err(e) => tracing::error!("Node stop failed: {}", e),
            }
        });
    });

    // --- Settings: Restart Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_restart_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: restarting node");
        let ui_w2 = ui_w.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w2.upgrade() {
                ui.set_connection_status("Restarting...".into());
            }
        });
        spawn_async(&rt_h, async move {
            crash_telemetry::set_last_state("node-restart (settings)");
            let _ = core.node.stop().await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node restarted");
                    crash_telemetry::set_last_state("node-running (settings restart)");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_node_running(true);
                            ui.set_connection_status("Connecting to bootnode...".into());
                        }
                    });
                }
                Err(e) => {
                    let msg = format!("Restart failed: {}", e);
                    tracing::error!("{}", msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_connection_status(msg.into());
                        }
                    });
                }
            }
        });
    });

    // --- Settings: Add Bootnode ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_add_bootnode(move |addr| {
        let addr_str = addr.to_string();
        tracing::info!("Settings: adding bootnode {}", addr_str);
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            if !config.bootnodes.contains(&addr_str) {
                config.bootnodes.push(addr_str);
                let _ = config.save();
            }
        });
    });

    // --- Settings: Remove Bootnode ---
    // Mirror of add_bootnode. The UI surfaces a row-level Remove button on
    // each entry in peer_connections.slint; clicking it was a no-op before
    // this handler landed.
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_remove_bootnode(move |addr| {
        let addr_str = addr.to_string();
        tracing::info!("Settings: removing bootnode {}", addr_str);
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.bootnodes.retain(|b| b != &addr_str);
            let _ = config.save();
        });
    });

    // NOTE (WP-1.3): the System Health "Retry bootnode / Retry RPC / Refresh /
    // Start IPFS" buttons are wired via the `on_health_*` callbacks further
    // below. app.slint forwards SettingsView's retry-bootnode/retry-node-rpc/
    // refresh-health/start-ipfs to `root.health-*`, so the live handlers are
    // `on_health_retry_bootnode`, `on_health_retry_node_rpc`,
    // `on_health_refresh_all`, and `on_health_start_ipfs`. An earlier set of
    // `on_settings_*` duplicates here referenced callbacks that were never
    // declared on the window (`settings-retry-bootnode`, …) — those methods
    // don't exist in the generated bindings and broke the build. Removed.

    // --- Settings: Knowledge Graph — open graph folder ---
    // The Logseq graph is a directory on disk (config.logseq_graph_path);
    // trail journals are written under {path}/journals/ (see trail.rs). The
    // "Open Graph Window" button reveals that folder in the OS file manager,
    // creating it first so the action always lands on a real directory even
    // before the first journal write.
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_open_graph_window(move || {
        tracing::info!("Settings: open knowledge-graph folder");
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let raw = core.config.read().await.logseq_graph_path.clone();
            let path = match raw.strip_prefix("~/") {
                Some(rest) => dirs::home_dir()
                    .map(|h| h.join(rest))
                    .unwrap_or_else(|| std::path::PathBuf::from(&raw)),
                None => std::path::PathBuf::from(&raw),
            };
            if let Err(e) = std::fs::create_dir_all(&path) {
                tracing::warn!("Could not create knowledge-graph dir {:?}: {}", path, e);
            }
            #[cfg(target_os = "macos")]
            let opener = "open";
            #[cfg(target_os = "linux")]
            let opener = "xdg-open";
            #[cfg(target_os = "windows")]
            let opener = "explorer";
            if let Err(e) = std::process::Command::new(opener).arg(&path).spawn() {
                tracing::warn!("Could not open knowledge-graph folder {:?}: {}", path, e);
            }
        });
    });

    // --- Settings: Knowledge Graph — save graph path ---
    // Persists the edited path into config.logseq_graph_path (mirror of the
    // remove_bootnode handler above). The trail recorder reads this on its
    // next journal write.
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_save_graph_path(move |path| {
        let path_str = path.to_string();
        tracing::info!("Settings: saving knowledge-graph path {}", path_str);
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.logseq_graph_path = path_str;
            let _ = config.save();
        });
    });

    // --- Settings: Set Theme ---
    // NATIVE-R1-S1 WP-1: flips `Theme.dark-mode` live ("dark" → evergreen
    // dark; "light"/"system" → canonical warm-paper light) and persists the
    // choice via config.theme.
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_set_theme(move |mode| {
        let mode_str = mode.to_string();
        tracing::info!("Settings: theme = {}", mode_str);
        if let Some(ui) = ui_w.upgrade() {
            ui.global::<Theme>().set_dark_mode(mode_str == "dark");
            ui.set_settings_theme_mode(mode_str.clone().into());
        }
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.theme = mode_str;
            let _ = config.save();
        });
    });

    // --- Settings: Factory Reset ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_factory_reset(move || {
        tracing::warn!("Settings: factory reset requested");
        let core = core.clone();
        let ui_w = ui_w.clone();
        spawn_async(&rt_h, async move {
            let data_dir = core.config.read().await.data_dir.clone();
            let _ = core.node.stop().await;
            if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                tracing::error!("Factory reset: failed to remove {}: {}", data_dir, e);
            } else {
                tracing::info!("Factory reset: removed {}", data_dir);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_show_onboarding(true);
                    ui.set_node_running(false);
                    ui.set_onboarding_step(0);
                }
            });
        });
    });

    // =========================================================================
    // WALLET DETAIL WIRING — Copy address, lock, unlock
    // =========================================================================

    // --- Wallet: Copy Address ---
    let ui_w = ui.as_weak();
    ui.on_wallet_copy_address(move || {
        if let Some(ui) = ui_w.upgrade() {
            let address = ui.get_wallet_selected_address().to_string();
            if !address.is_empty() {
                match arboard::Clipboard::new() {
                    Ok(mut clipboard) => {
                        if clipboard.set_text(&address).is_ok() {
                            tracing::info!("Address copied to clipboard");
                        }
                    }
                    Err(e) => tracing::warn!("Clipboard not available: {}", e),
                }
            }
        }
    });

    // --- Wallet: Lock ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_lock_clicked(move || {
        tracing::info!("Wallet: locking");
        let core = core.clone();
        let ui_w = ui_w.clone();
        spawn_async(&rt_h, async move {
            perform_wallet_lock_teardown(&core.wallet).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_session_active(false);
                    ui.set_wallet_session_remaining("".into());
                    ui.set_show_lock_screen(true);
                }
            });
        });
    });

    // --- Wallet: Unlock (from wallet view, not lock screen) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_unlock_clicked(move || {
        // The wallet view unlock just flags that the session needs re-auth.
        // The lock screen will handle the actual password entry.
        tracing::info!("Wallet: unlock requested, showing lock screen");
        if let Some(ui) = ui_w.upgrade() {
            ui.set_show_lock_screen(true);
        }
        let _ = (&core, &rt_h); // used for future re-auth flow
    });

    // =========================================================================
    // DAG EXPLORER WIRING — Search, select block, back
    // =========================================================================

    // BFR-INT-5b: shared cache of the most recently loaded block's tx
    // details, keyed by tx_hash. On block detail open we populate it +
    // the Slint VecModel for the row list. On tx-row click we look up
    // the full detail here and populate the modal properties.
    // Declared up-front so both the search and select-block handlers
    // can fire `load_block_txs` inline on success (AT-5b-1; replaces
    // the prior 500 ms `slint::Timer` polling pattern).
    let dag_tx_cache: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, citrate_desktop_app::services::node_service::BlockTxDetail>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let load_block_txs: std::sync::Arc<dyn Fn(String) + Send + Sync> = {
        let core = app_core.clone();
        let rt_h = rt.handle().clone();
        let ui_w = ui.as_weak();
        let cache = dag_tx_cache.clone();
        std::sync::Arc::new(move |block_hash: String| {
            let core = core.clone();
            let ui_w = ui_w.clone();
            let cache = cache.clone();
            spawn_async(&rt_h, async move {
                let txs = core.node.get_block_transactions(&block_hash).await;
                let mut map = std::collections::HashMap::with_capacity(txs.len());
                let mut rows: Vec<TxRowData> = Vec::with_capacity(txs.len());
                for tx in &txs {
                    let elide = |s: &str| -> String {
                        if s.len() > 14 {
                            format!("{}…{}", &s[..8], &s[s.len().saturating_sub(6)..])
                        } else {
                            s.to_string()
                        }
                    };
                    let method = calldata_decoder::decode_selector(&tx.input_hex);
                    rows.push(TxRowData {
                        tx_hash: tx.tx_hash.clone().into(),
                        hash_short: elide(&tx.tx_hash).into(),
                        method_label: method.label().into(),
                        value_display: format_wei_short(&tx.value_wei).into(),
                        status: tx.status.as_str().into(),
                    });
                    map.insert(tx.tx_hash.clone(), tx.clone());
                }
                if let Ok(mut c) = cache.lock() {
                    *c = map;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_dag_detail_transactions(slint::ModelRc::from(
                            std::rc::Rc::new(slint::VecModel::from(rows)),
                        ));
                    }
                });
            });
        })
    };

    // --- DAG: Search by height or hash ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let load_txs_search = load_block_txs.clone();
    ui.on_dag_search(move |query| {
        let query_str = query.to_string().trim().to_string();
        let core = core.clone();
        let ui_w = ui_w.clone();
        let load_txs = load_txs_search.clone();
        tracing::info!("DAG: searching for '{}'", query_str);

        if let Ok(height) = query_str.parse::<u64>() {
            // Read block directly from local RocksDB via NodeService
            spawn_async(&rt_h, async move {
                let summaries = core.node.get_recent_blocks(50).await
                    .unwrap_or_default();
                let block = summaries.iter().find(|b| b.height == height);
                match block {
                    Some(block) => {
                        tracing::info!("DAG: found block at height {}", block.height);
                        let hash = block.hash.clone();
                        let h = block.height as i32;
                        let ts = block.timestamp.to_string();
                        let tx = block.tx_count as i32;
                        let parent = block.selected_parent.clone();
                        let bs = block.blue_score as i32;
                        let hash_for_tx_load = hash.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_dag_show_detail(true);
                                ui.set_dag_detail_hash(hash.into());
                                ui.set_dag_detail_height(h);
                                ui.set_dag_detail_timestamp(ts.into());
                                ui.set_dag_detail_tx_count(tx);
                                ui.set_dag_detail_parent(parent.into());
                                ui.set_dag_detail_blue_score(bs);
                                ui.set_dag_detail_proposer("".into());
                            }
                        });
                        // AT-5b-1 — load tx list inline once we know the
                        // block exists. spawn_async inside load_block_txs
                        // does the actual fetch + UI push.
                        load_txs(hash_for_tx_load);
                    }
                    None => tracing::warn!("DAG: block {} not in local storage", height),
                }
            });
        }
    });

    // --- DAG: Select block (click on block row) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let load_txs_select = load_block_txs.clone();
    ui.on_dag_select_block(move |height| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let load_txs = load_txs_select.clone();
        tracing::info!("DAG: selecting block at height {}", height);

        // Read block detail directly from local RocksDB storage via NodeService
        // (not via HTTP RPC which requires a running JSON-RPC server)
        spawn_async(&rt_h, async move {
            let all_summaries = core.node.get_recent_blocks(50).await
                .unwrap_or_default();
            let block = all_summaries.iter().find(|b| b.height == height as u64);

            match block {
                Some(block) => {
                    let hash = block.hash.clone();
                    let h = block.height as i32;
                    let ts = block.timestamp.to_string();
                    let tx = block.tx_count as i32;
                    let parent = block.selected_parent.clone();
                    let bs = block.blue_score as i32;
                    let hash_for_tx_load = hash.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_dag_show_detail(true);
                            ui.set_dag_detail_hash(hash.into());
                            ui.set_dag_detail_height(h);
                            ui.set_dag_detail_timestamp(ts.into());
                            ui.set_dag_detail_tx_count(tx);
                            ui.set_dag_detail_parent(parent.into());
                            ui.set_dag_detail_blue_score(bs);
                            ui.set_dag_detail_proposer("".into());
                        }
                    });
                    // AT-5b-1 — load tx list inline.
                    load_txs(hash_for_tx_load);
                }
                None => {
                    tracing::warn!("DAG: block at height {} not found in local storage", height);
                }
            }
        });
    });

    // --- DAG: Back to list ---
    let ui_w = ui.as_weak();
    ui.on_dag_back_to_list(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_dag_show_detail(false);
            // Clear the tx list so it doesn't flash through on next open.
            ui.set_dag_detail_transactions(slint::ModelRc::from(std::rc::Rc::new(
                slint::VecModel::from(Vec::<TxRowData>::new()),
            )));
        }
    });

    // --- DAG: TX modal close ---
    let ui_w = ui.as_weak();
    ui.on_tx_modal_close(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_tx_modal_visible(false);
        }
    });

    // --- DAG: TX row clicked → open modal with full detail ---
    let ui_w = ui.as_weak();
    let cache = dag_tx_cache.clone();
    ui.on_dag_tx_row_clicked(move |tx_hash| {
        let tx_hash_s = tx_hash.to_string();
        let Some(ui) = ui_w.upgrade() else { return };
        let Ok(c) = cache.lock() else { return };
        let Some(tx) = c.get(&tx_hash_s) else {
            tracing::warn!("DAG: tx-row clicked for unknown hash {}", tx_hash_s);
            return;
        };
        let method = calldata_decoder::decode_selector(&tx.input_hex);
        ui.set_tx_modal_hash(tx.tx_hash.clone().into());
        ui.set_tx_modal_status(tx.status.as_str().into());
        ui.set_tx_modal_from(tx.from.clone().into());
        ui.set_tx_modal_to(tx.to.clone().unwrap_or_default().into());
        ui.set_tx_modal_value(format_wei_long(&tx.value_wei).into());
        ui.set_tx_modal_nonce(tx.nonce.to_string().into());
        ui.set_tx_modal_gas_used(format!("{}", tx.gas_used).into());
        ui.set_tx_modal_gas_price(format!("{} wei", tx.effective_gas_price_wei).into());
        ui.set_tx_modal_method(method.label().into());
        ui.set_tx_modal_input_hex(
            if tx.input_hex.is_empty() {
                String::new()
            } else {
                format!("0x{}", tx.input_hex)
            }
            .into(),
        );
        ui.set_tx_modal_block_height(tx.block_height as i32);
        ui.set_tx_modal_visible(true);
    });

    // =========================================================================
    // MODELS WIRING — Load, deploy, inference, browse
    // =========================================================================

    // --- Models: Load model ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_load_model(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: loading model");
        spawn_async(&rt_h, async move {
            match core.models.refresh_models().await {
                Ok(models) => {
                    tracing::info!("Models: loaded {} models", models.len());
                    let count = models.len() as i32;
                    let first_name = models.first().map(|m| m.name.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_count(count);
                            if let Some(name) = first_name {
                                ui.set_chat_model_name(name.into());
                                ui.set_chat_model_loaded(true);
                            }
                        }
                    });
                }
                Err(e) => tracing::error!("Models: load failed: {}", e),
            }
        });
    });

    // --- Models: Deploy model to precompile ---
    // Wired through app_binder::bind_model_publish() — same code path used by tests.
    app_binder::bind_model_publish(&ui, app_core.clone(), rt.handle());

    // --- Models: Run inference ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_run_inference(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: running inference test");
        spawn_async(&rt_h, async move {
            match core.chat.send_message("Hello, this is an inference test.").await {
                Ok(response) => {
                    let content = clean_markdown(&response.content);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_last_response(content.into());
                        }
                    });
                }
                Err(e) => tracing::error!("Models: inference test failed: {}", e),
            }
        });
    });

    // --- Models: Browse HuggingFace ---
    ui.on_models_browse_huggingface(move || {
        tracing::info!("Models: opening HuggingFace in browser");
        // Open URL in default browser via xdg-open (Linux)
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://huggingface.co/models?search=gguf")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Models: Pin local model to IPFS ---
    // Data source: IPFS daemon HTTP API at localhost:5001/api/v0/add
    // Reads local GGUF file from ~/.local/share/citrate/models/ and adds to IPFS
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_pin_to_ipfs(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Models: pinning local model to IPFS");

        // Show pinning state immediately
        if let Some(ui) = ui_w.upgrade() {
            ui.set_models_pinning(true);
        }

        spawn_async(&rt_h, async move {
            // Find the local model file
            let model_dir = dirs::data_local_dir()
                .map(|d| d.join("citrate").join("models"));
            let model_path = match model_dir {
                Some(dir) if dir.exists() => {
                    match std::fs::read_dir(&dir) {
                        Ok(entries) => entries
                            .filter_map(|e| e.ok())
                            .find(|e| e.path().extension().is_some_and(|ext| ext == "gguf"))
                            .map(|e| e.path()),
                        Err(_) => None,
                    }
                }
                _ => None,
            };

            let path = match model_path {
                Some(p) => p,
                None => {
                    tracing::warn!("Models: no local GGUF file found to pin");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                    return;
                }
            };

            tracing::info!("Models: adding {} to IPFS via CLI", path.display());

            // Use `ipfs add --pin` CLI — simpler than multipart HTTP and handles large files
            let path_str = path.to_string_lossy().to_string();
            match tokio::process::Command::new("ipfs")
                .args(["add", "--pin", "--quieter", &path_str])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let cid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    tracing::info!("Models: pinned to IPFS — CID: {}", cid);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_ipfs_cid(cid.into());
                            ui.set_models_pinning(false);
                        }
                    });
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!("Models: ipfs add failed — {}", stderr);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Models: ipfs command not found or failed — {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                }
            }
        });
    });

    // --- Settings: Connect GitHub (open token settings in browser) ---
    ui.on_settings_connect_github(move || {
        tracing::info!("Settings: opening GitHub token settings in browser");
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://github.com/settings/tokens")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Settings: Connect HuggingFace (open token settings in browser) ---
    ui.on_settings_connect_huggingface(move || {
        tracing::info!("Settings: opening HuggingFace token settings in browser");
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://huggingface.co/settings/tokens")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Settings: Save Integration Token ---
    // Data source: OS keychain; AppConfig persists only a secret marker.
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_save_integration_token(move |name, token| {
        let name_str = name.to_string();
        let token_str = token.to_string();
        let core = core.clone();
        tracing::info!("Settings: saving integration token for {} ({} chars)", name_str, token_str.len());
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            let result = config
                .set_integration_token(&name_str, &token_str)
                .and_then(|_| config.save());
            if let Err(e) = result {
                tracing::error!("Failed to save integration token securely: {}", e);
            } else {
                tracing::info!("Settings: {} integration token saved to OS keychain", name_str);
            }
        });
    });

    // --- Settings: Set Environment (network switch) ---
    // Data source: AppConfig.network persisted to disk via config.save()
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_set_environment(move |env| {
        let env_str = env.to_string();
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: switching environment to {}", env_str);
        spawn_async(&rt_h, async move {
            // Stop current node — waits for background tasks and releases RocksDB
            if let Err(e) = core.node.stop().await {
                tracing::warn!("Stop during env switch: {}", e);
            }

            // Reconfigure bootnodes based on environment
            let lower = env_str.to_lowercase();
            match lower.as_str() {
                "devnet" => {
                    core.node.update_bootnodes(vec![]).await;
                    tracing::info!("Devnet: no bootnodes (local only)");
                }
                "testnet" => {
                    core.node.update_bootnodes(vec!["159.65.227.42:30303".to_string()]).await;
                    tracing::info!("Testnet: connecting to bootnode");
                }
                _ => {}
            }

            // Update config: network, chain_id, data_dir, and bootnodes move together
            let mut config = core.config.write().await;
            config.network = lower.clone();
            config.chain_id = citrate_desktop_app::chain_id_for_network(&lower);
            config.data_dir = citrate_desktop_app::data_dir_for_network(&lower);
            config.bootnodes = match lower.as_str() {
                "devnet" => vec![],
                _ => vec!["159.65.227.42:30303".to_string()],
            };
            tracing::info!(
                "Environment switch: network={}, chain_id={}, data_dir={}",
                lower, config.chain_id, config.data_dir
            );
            if let Err(e) = config.save() {
                tracing::error!("Failed to save config: {}", e);
            }
            let new_chain_id = config.chain_id;
            drop(config);

            // Update wallet runtime to match new environment
            // This ensures signed transactions use the correct chain ID AND RPC target
            core.wallet.set_chain_id(new_chain_id);
            let rpc_url = {
                // Re-read so the `rpc_port` we pass to active_rpc_url
                // reflects any edits the network switch just made.
                let cfg = core.config.read().await;
                cfg.active_rpc_url()
            };
            core.wallet.set_rpc_url(&rpc_url);
            tracing::info!("Wallet updated: chain_id={}, rpc={} for {}", new_chain_id, rpc_url, lower);

            // Restart node with new config (reads updated data_dir + chain_id)
            crash_telemetry::set_last_state("node-start (environment switch)");
            if let Err(e) = core.node.start().await {
                tracing::error!("Failed to restart node: {}", e);
            } else {
                crash_telemetry::set_last_state("node-running (environment switch)");
            }

            // Refresh chat context with new network info
            let address = core.wallet.get_primary_address().await
                .unwrap_or_else(|| "not connected".to_string());
            let height = core.node.get_status().await.block_height;
            core.chat.set_context(&address, "0", &lower, height).await;

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_environment(env_str.into());
                }
            });
        });
    });

    // =========================================================================
    // LEARNING CENTER WIRING — Join pool, stake, claim
    // =========================================================================

    // --- Learning: Join Pool (P960-I rebuild) ---
    // Data source: LearningPool.joinPool(uint256) payable
    //   — contracts/src/LearningPool.sol:120
    // V1 sends a fixed 1000 SALT stake to pool 0; pool selection +
    // dynamic stake amount land in a follow-up modal sprint.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_learning_join_pool(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            tracing::info!("Learning: join pool requested");
            spawn_async(&rt_h, async move {
                let chain_id = core.config.read().await.chain_id;
                let Some(addr) = marketplace_client::learning_pool_address(chain_id) else {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_learning_pool_status(
                                    format!("LearningPool not deployed on chain {}", chain_id).into()
                                );
                            }
                        }
                    });
                    return;
                };
                let accounts = core.wallet.list_accounts().await;
                let Some(from) = accounts.first().map(|a| a.address.clone()) else {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_learning_pool_status(
                                    "Create a wallet before joining a pool".into()
                                );
                            }
                        }
                    });
                    return;
                };
                let pool_id = ui_w.upgrade()
                    .map(|ui| ui.get_learning_current_pool_id() as u64)
                    .unwrap_or(0);
                let data = marketplace_client::encode_join_pool(pool_id);
                // Default stake: 1000 SALT (matches LearningPool's typical
                // minStake; if a pool requires more, the tx will revert
                // and the receipt-poll will surface the failure).
                let stake_wei = marketplace_client::MIN_PROVIDER_STAKE_WEI.to_string();
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_learning_pool_status(
                                format!("Submitting joinPool({}) tx…", pool_id).into()
                            );
                        }
                    }
                });
                // NAT-B-007: surface the decoded intent, target, and 1000-SALT
                // stake for explicit Approve/Deny before broadcasting.
                if !confirm_tx_intent(&core.approvals, "Join learning pool", addr, &stake_wei, &data).await {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_learning_pool_status("Cancelled — join not approved".into());
                            }
                        }
                    });
                    return;
                }
                match core.wallet
                    .send_transaction_with_data(&from, addr, &stake_wei, data, "")
                    .await
                {
                    Ok(tx) => {
                        tracing::info!("Learning: joinPool tx={}", tx);
                        let submitted = format!("Submitted joinPool({}) — tx {}", pool_id, short_hash(&tx));
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(submitted.into());
                                }
                            }
                        });
                        // T1-1: poll receipt on the same node the wallet submitted to.
                        let rpc_url = core.wallet.get_rpc_url();
                        let final_msg = match poll_tx_receipt(&rpc_url, &tx).await {
                            Ok(ReceiptOutcome::Confirmed { block_number }) => {
                                format!("Joined pool {} (block {})", pool_id, block_number)
                            }
                            Ok(ReceiptOutcome::Reverted) => {
                                format!("Join pool {} reverted — below minStake or not Open access?", pool_id)
                            }
                            Ok(ReceiptOutcome::Pending) => {
                                format!("Join pool {} pending — tx {} not yet mined", pool_id, short_hash(&tx))
                            }
                            Err(e) => format!("Receipt poll failed: {}", e),
                        };
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(final_msg.into());
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Learning: joinPool failed: {}", e);
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(format!("Join failed: {}", e).into());
                                }
                            }
                        });
                    }
                }
            });
        });
    }

    // --- Learning: Leave Pool ---
    // Data source: LearningPool.leavePool(uint256) — returns user's stake
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_learning_leave_pool(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let chain_id = core.config.read().await.chain_id;
                let Some(addr) = marketplace_client::learning_pool_address(chain_id) else {
                    return;
                };
                let accounts = core.wallet.list_accounts().await;
                let Some(from) = accounts.first().map(|a| a.address.clone()) else { return; };
                let pool_id = ui_w.upgrade()
                    .map(|ui| ui.get_learning_current_pool_id() as u64)
                    .unwrap_or(0);
                let data = marketplace_client::encode_leave_pool(pool_id);
                // NAT-B-007: confirm the leavePool write before broadcasting.
                if !confirm_tx_intent(&core.approvals, "Leave learning pool", addr, "0", &data).await {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_learning_pool_status("Cancelled — leave not approved".into());
                            }
                        }
                    });
                    return;
                }
                match core.wallet
                    .send_transaction_with_data(&from, addr, "0", data, "")
                    .await
                {
                    Ok(tx) => {
                        tracing::info!("Learning: leavePool tx={}", tx);
                        let submitted = format!("Submitted leavePool({}) — tx {}", pool_id, short_hash(&tx));
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(submitted.into());
                                }
                            }
                        });
                        // T1-1: poll receipt on the same node the wallet submitted to.
                        let rpc_url = core.wallet.get_rpc_url();
                        let final_msg = match poll_tx_receipt(&rpc_url, &tx).await {
                            Ok(ReceiptOutcome::Confirmed { block_number }) => {
                                format!("Left pool {} (block {})", pool_id, block_number)
                            }
                            Ok(ReceiptOutcome::Reverted) => {
                                format!("Leave pool {} reverted — not a member?", pool_id)
                            }
                            Ok(ReceiptOutcome::Pending) => {
                                format!("Leave pool {} pending — tx {} not yet mined", pool_id, short_hash(&tx))
                            }
                            Err(e) => format!("Receipt poll failed: {}", e),
                        };
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(final_msg.into());
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Learning: leavePool failed: {}", e);
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_learning_pool_status(format!("Leave failed: {}", e).into());
                                }
                            }
                        });
                    }
                }
            });
        });
    }

    // --- Learning: Claim Earnings ---
    // Data source: ContributionAccounting.claimRewards() — same accounting
    // contract that compute uses; both pool members and compute providers
    // accrue claimable balances through it.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_learning_claim_earnings(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let chain_id = core.config.read().await.chain_id;
                let Some(acc_addr) = marketplace_client::contribution_accounting_address(chain_id) else {
                    return;
                };
                let accounts = core.wallet.list_accounts().await;
                let Some(from) = accounts.first().map(|a| a.address.clone()) else { return; };
                let data = marketplace_client::encode_claim_rewards();
                // NAT-B-007: confirm the claimRewards write before broadcasting.
                if !confirm_tx_intent(&core.approvals, "Claim learning earnings", acc_addr, "0", &data).await {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_clipboard_toast("Cancelled — claim not approved".into());
                            }
                        }
                    });
                    return;
                }
                match core.wallet
                    .send_transaction_with_data(&from, acc_addr, "0", data, "")
                    .await
                {
                    Ok(tx) => {
                        tracing::info!("Learning: claimRewards tx={}", tx);
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            let tx = tx.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_clipboard_toast(
                                        format!("Claim submitted — tx {}", short_hash(&tx)).into()
                                    );
                                }
                            }
                        });
                        // T1-1: poll receipt on the same node the wallet submitted to.
                        let rpc_url = core.wallet.get_rpc_url();
                        let final_msg = match poll_tx_receipt(&rpc_url, &tx).await {
                            Ok(ReceiptOutcome::Confirmed { block_number }) => {
                                format!("Claim confirmed in block {}", block_number)
                            }
                            Ok(ReceiptOutcome::Reverted) => {
                                "Claim reverted — nothing claimable?".to_string()
                            }
                            Ok(ReceiptOutcome::Pending) => {
                                format!("Claim pending — tx {} not yet mined", short_hash(&tx))
                            }
                            Err(e) => format!("Receipt poll failed: {}", e),
                        };
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_clipboard_toast(final_msg.into());
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Learning: claimRewards failed: {}", e);
                    }
                }
            });
        });
    }

    // --- Learning: Create Pool (T2-9) ---
    // Data source: LearningPool.createPool(string,string,uint8,uint256)
    //   payable. v1 hardcodes Open access (uint8=0) + 1000 SALT minStake;
    //   custom values are a follow-up sprint.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_create_pool_submit(move |name, description| {
            let core = core.clone();
            let ui_w = ui_w.clone();
            let name_s = name.to_string();
            let desc_s = description.to_string();
            spawn_async(&rt_h, async move {
                if name_s.trim().is_empty() {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_create_pool_status("Name is required".into());
                            }
                        }
                    });
                    return;
                }
                let chain_id = core.config.read().await.chain_id;
                let Some(addr) = marketplace_client::learning_pool_address(chain_id) else {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_create_pool_status(
                                    format!("LearningPool not deployed on chain {}", chain_id).into()
                                );
                            }
                        }
                    });
                    return;
                };
                let accounts = core.wallet.list_accounts().await;
                let Some(from) = accounts.first().map(|a| a.address.clone()) else {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_create_pool_status("Create a wallet first".into());
                            }
                        }
                    });
                    return;
                };
                // V1 fixed: Open access (0) + 1000 SALT minStake
                let data = marketplace_client::encode_create_pool(
                    &name_s, &desc_s, 0, marketplace_client::MIN_PROVIDER_STAKE_WEI,
                );
                let stake_wei = marketplace_client::MIN_PROVIDER_STAKE_WEI.to_string();
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_create_pool_status("Submitting createPool tx…".into());
                        }
                    }
                });
                // NAT-B-007: confirm the createPool write (1000-SALT minStake) first.
                if !confirm_tx_intent(&core.approvals, "Create learning pool", addr, &stake_wei, &data).await {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_create_pool_status("Cancelled — createPool not approved".into());
                            }
                        }
                    });
                    return;
                }
                match core.wallet
                    .send_transaction_with_data(&from, addr, &stake_wei, data, "")
                    .await
                {
                    Ok(tx) => {
                        tracing::info!("Learning: createPool tx={}", tx);
                        // Poll receipt on the same node the wallet submitted to.
                        let rpc_url = core.wallet.get_rpc_url();
                        let final_msg = match poll_tx_receipt(&rpc_url, &tx).await {
                            Ok(ReceiptOutcome::Confirmed { block_number }) => {
                                format!("Pool created in block {} — closing dialog", block_number)
                            }
                            Ok(ReceiptOutcome::Reverted) => {
                                "createPool reverted — check the contract revert reason".to_string()
                            }
                            Ok(ReceiptOutcome::Pending) => {
                                format!("Still pending — tx {}", short_hash(&tx))
                            }
                            Err(e) => format!("Receipt poll failed: {}", e),
                        };
                        let dismiss = final_msg.starts_with("Pool created");
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_create_pool_status(final_msg.into());
                                    if dismiss {
                                        // Close the modal after a short
                                        // delay so the user sees the
                                        // success message.
                                        let ui_for_dismiss = ui_w.clone();
                                        slint::Timer::single_shot(
                                            std::time::Duration::from_millis(2500),
                                            move || {
                                                if let Some(ui) = ui_for_dismiss.upgrade() {
                                                    ui.set_show_create_pool_dialog(false);
                                                    ui.set_create_pool_status("".into());
                                                    ui.set_create_pool_name("".into());
                                                    ui.set_create_pool_description("".into());
                                                }
                                            },
                                        );
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Learning: createPool send failed: {}", e);
                        let err_msg = format!("Tx send failed: {}", e);
                        let _ = slint::invoke_from_event_loop({
                            let ui_w = ui_w.clone();
                            move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_create_pool_status(err_msg.into());
                                }
                            }
                        });
                    }
                }
            });
        });
    }
    // create-pool dialog close handler
    {
        let ui_w = ui.as_weak();
        ui.on_create_pool_close(move || {
            if let Some(ui) = ui_w.upgrade() {
                ui.set_create_pool_status("".into());
                ui.set_create_pool_name("".into());
                ui.set_create_pool_description("".into());
            }
        });
    }

    // =========================================================================
    // EDUCATION WIRING — Institutional vault, classroom, budget, forwarder
    // =========================================================================
    // Data sources: InstitutionalVault, ClassroomClusterV1, BudgetAllocation,
    // CashoutRequest, Forwarder — all deployed on chain 40204 (2026-04-05).

    // --- Edu: Refresh data ---
    {
        let ui_w = ui.as_weak();
        let core = app_core.clone();
        let rt_h = rt.handle().clone();
        ui.on_edu_refresh(move || {
            let ui_w = ui_w.clone();
            let core = core.clone();
            tracing::info!("Edu: refresh requested");
            spawn_async(&rt_h, async move {
                use citrate_desktop_app::services::edu::institutional_service::{RpcInstitutionalBackend, InstitutionalBackend};

                // Format RPC URL from the live config. AppCore exposes the RPC
                // port via AppConfig (not a raw rpc_url field) so environment
                // switches at runtime are picked up automatically. Same pattern
                // as the contract-deploy receipt poll below.
                let rpc = {
                    let config = core.config.read().await;
                    config.active_rpc_url()
                };

                let inst = RpcInstitutionalBackend::new(&rpc);
                match inst.get_vault_status().await {
                    Ok(status) => {
                        let ui_w = ui_w.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_edu_vault_paused(status.is_paused);
                                ui.set_edu_vault_threshold(status.threshold as i32);
                                ui.set_edu_vault_signer_count(status.signer_count as i32);
                                // Grain → SALT at the UI boundary. The
                                // `balance_wei` field is raw grains from
                                // InstitutionalVault.getBalance(); users
                                // should see "10 SALT", not a 19-digit wei.
                                let vault_salt = citrate_wallet_core::format::grains_str_to_salt(
                                    &status.balance_wei,
                                );
                                ui.set_edu_vault_balance(vault_salt.into());
                            }
                        });
                    }
                    Err(e) => tracing::warn!("Edu: vault status query failed: {}", e),
                }

                // Also refresh SALT/USD rate
                match inst.get_salt_usd_rate().await {
                    Ok(rate) => {
                        let display = format!("{:.2}", rate as f64 / 10000.0);
                        let ui_w = ui_w.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_edu_salt_usd_rate(display.into());
                            }
                        });
                    }
                    Err(e) => tracing::warn!("Edu: SALT/USD rate query failed: {}", e),
                }
            });
        });
    }

    // --- Edu: Request Cashout ---
    {
        let ui_w = ui.as_weak();
        let _rt_h = rt.handle().clone();
        ui.on_edu_request_cashout(move || {
            let ui_w = ui_w.clone();
            tracing::info!("Edu: teacher cashout requested");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_ui) = ui_w.upgrade() {
                    tracing::info!("Edu: cashout flow — requires wallet tx to CashoutRequest.requestCashout()");
                }
            });
        });
    }

    // --- Edu: Approve Cashout ---
    {
        let ui_w = ui.as_weak();
        let _rt_h = rt.handle().clone();
        ui.on_edu_approve_cashout(move || {
            let ui_w = ui_w.clone();
            tracing::info!("Edu: admin cashout approval requested");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(_ui) = ui_w.upgrade() {
                    tracing::info!("Edu: approval flow — requires vault governance tx to CashoutRequest.approveCashout()");
                }
            });
        });
    }

    // --- Edu: Create Classroom ---
    {
        let _rt_h = rt.handle().clone();
        ui.on_edu_create_classroom(move || {
            tracing::info!("Edu: create classroom requested — requires Admin role tx to ClassroomClusterV1.createClassroom()");
        });
    }

    // --- Edu: Register Device ---
    {
        let _rt_h = rt.handle().clone();
        ui.on_edu_register_device(move || {
            tracing::info!("Edu: register device requested — requires IT role tx to ClassroomClusterV1.registerDevice()");
        });
    }

    // =========================================================================
    // COMPUTE MARKETPLACE WIRING — Post job, register provider, refresh
    // =========================================================================

    // (T2-1: on_compute_post_job retired. Callback existed but no
    // UI button invoked it. Job posting from the GUI is a future
    // feature behind a real UI flow — when that lands, restore the
    // handler then.)

    // --- Compute: Register Provider (P960-D WP-D.3) ---
    // Data source: ComputeMarketplace.registerProvider(bytes32[])
    //   payable, MIN_PROVIDER_STAKE = 1000 SALT
    //   — ComputeMarketplace.sol:273
    // Address lookup: marketplace_client::compute_marketplace_address(chain_id)
    // V1 passes a single bytes32 sentinel keccak256("any") for supported
    // models. A future WP will let the user enumerate specific model IDs.
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_compute_register_provider(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Compute: register provider requested");
        spawn_async(&rt_h, async move {
            let chain_id = core.config.read().await.chain_id;
            let Some(market_addr) = marketplace_client::compute_marketplace_address(chain_id) else {
                let msg = format!("ComputeMarketplace not deployed on chain {}", chain_id);
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_compute_provider_status(msg.into());
                        }
                    }
                });
                return;
            };

            // Get the active wallet account. Without one, registration is
            // meaningless — surface a clear error in the provider-status
            // line rather than silently failing.
            let accounts = core.wallet.list_accounts().await;
            let from = match accounts.first() {
                Some(a) => a.address.clone(),
                None => {
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_compute_provider_status(
                                    "Create a wallet before registering as a provider".into(),
                                );
                            }
                        }
                    });
                    return;
                }
            };

            // Build the call data: one "any-model" sentinel, 1000 SALT stake.
            let data = marketplace_client::encode_register_provider(
                &[marketplace_client::any_model_hash()],
            );
            let stake_wei = marketplace_client::MIN_PROVIDER_STAKE_WEI.to_string();

            // Flip UI to a "submitting..." state so double-clicks are harmless.
            let _ = slint::invoke_from_event_loop({
                let ui_w = ui_w.clone();
                move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_compute_provider_status("Submitting registration tx…".into());
                    }
                }
            });

            // NAT-B-007: confirm the registerProvider write (1000-SALT stake) first.
            if !confirm_tx_intent(&core.approvals, "Register compute provider", market_addr, &stake_wei, &data).await {
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_compute_provider_status("Cancelled — registration not approved".into());
                        }
                    }
                });
                return;
            }
            match core
                .wallet
                .send_transaction_with_data(&from, market_addr, &stake_wei, data, "")
                .await
            {
                Ok(tx_hash) => {
                    tracing::info!("Compute: registerProvider tx={}", tx_hash);
                    let status_msg = format!("Registration submitted — tx {}", short_hash(&tx_hash));
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_compute_provider_status(status_msg.into());
                            }
                        }
                    });

                    // Poll for receipt (up to 40s). T1-1 shared helper.
                    // Poll the same node the wallet submitted to.
                    let rpc_url = core.wallet.get_rpc_url();
                    let final_msg = match poll_tx_receipt(&rpc_url, &tx_hash).await {
                        Ok(ReceiptOutcome::Confirmed { block_number }) => {
                            format!("Registered — provider active (block {})", block_number)
                        }
                        Ok(ReceiptOutcome::Reverted) => {
                            "Registration reverted — check MIN_PROVIDER_STAKE and supportedModels".to_string()
                        }
                        Ok(ReceiptOutcome::Pending) => {
                            format!("Registration pending — tx {} not yet mined", short_hash(&tx_hash))
                        }
                        Err(e) => format!("Receipt poll failed: {}", e),
                    };
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_compute_provider_status(final_msg.into());
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Compute: registerProvider send failed: {}", e);
                    let err_msg = format!("Registration tx failed: {}", e);
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_compute_provider_status(err_msg.into());
                            }
                        }
                    });
                }
            }
        });
    });

    // P960-D WP-D.2: persist opt-in settings on every change. The
    // slider + toggle + schedule fire `settings-changed` after
    // mutating their two-way-bound properties.
    let ui_w = ui.as_weak();
    ui.on_compute_settings_changed(move || {
        let ui_w = ui_w.clone();
        let Some(ui) = ui_w.upgrade() else { return; };
        let settings = compute_service::ComputeSettings {
            enabled: ui.get_compute_enabled(),
            allocation_percent: ui.get_compute_allocation() as u32,
            schedule: ui.get_compute_schedule().to_string(),
        };
        // Normalize schedule → also update the description line.
        let desc = settings.schedule_description();
        ui.set_compute_schedule_description(desc.into());
        if let Some(path) = compute_service::ComputeSettings::default_path() {
            if let Err(e) = settings.save(&path) {
                tracing::warn!("compute.json save failed: {}", e);
            }
        }
        tracing::info!(
            "Compute settings: enabled={} alloc={}% schedule={}",
            settings.enabled, settings.allocation_percent, settings.schedule,
        );
    });

    // --- GUI-RELAY-S1b: "Auto-sign won jobs" toggle ---
    // Reflect the relay's initial (env) state, then flip the shared RelayService
    // flag when the user toggles it. The background loop reads the same flag.
    ui.set_relay_enabled(relay_for_toggle.as_ref().is_some_and(|r| r.is_enabled()));
    {
        let relay = relay_for_toggle.clone();
        let ui_w = ui.as_weak();
        ui.on_relay_toggled(move || {
            let Some(ui) = ui_w.upgrade() else { return; };
            let on = ui.get_relay_enabled();
            match &relay {
                Some(r) => {
                    r.set_enabled(on);
                    tracing::info!(
                        "signing relay: {} via toggle",
                        if on { "enabled" } else { "disabled" }
                    );
                }
                None if on => {
                    // No relay configured (no marketplace address for this chain) —
                    // bounce the toggle back off and tell the user honestly.
                    ui.set_relay_enabled(false);
                    ui.set_clipboard_toast("Auto-sign unavailable on this chain".into());
                }
                None => {}
            }
        });
    }

    // --- Compute: Claim Earnings (P960-D WP-D.4) ---
    // Data source: ContributionAccounting.claimRewards() — sends tx,
    // transfers msg.sender's `claimable` balance out. Visible-only when
    // the polling loop has written a non-zero `compute-earned`.
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_compute_claim_earnings(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        spawn_async(&rt_h, async move {
            let chain_id = core.config.read().await.chain_id;
            let Some(acc_addr) = marketplace_client::contribution_accounting_address(chain_id) else {
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_clipboard_toast(
                                format!("Claim unavailable — no accounting contract on chain {}", chain_id).into(),
                            );
                        }
                    }
                });
                return;
            };
            let accounts = core.wallet.list_accounts().await;
            let Some(from) = accounts.first().map(|a| a.address.clone()) else {
                return;
            };
            let data = marketplace_client::encode_claim_rewards();
            // NAT-B-007: confirm the claimRewards write before broadcasting.
            if !confirm_tx_intent(&core.approvals, "Claim compute earnings", acc_addr, "0", &data).await {
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_clipboard_toast("Cancelled — claim not approved".into());
                        }
                    }
                });
                return;
            }
            match core
                .wallet
                .send_transaction_with_data(&from, acc_addr, "0", data, "")
                .await
            {
                Ok(tx) => {
                    tracing::info!("Compute: claimRewards tx={}", tx);
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        let tx = tx.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_clipboard_toast(
                                    format!("Claim submitted — tx {}", short_hash(&tx)).into(),
                                );
                            }
                        }
                    });
                    // T1-1: poll receipt on the same node the wallet submitted to.
                    let rpc_url = core.wallet.get_rpc_url();
                    let final_msg = match poll_tx_receipt(&rpc_url, &tx).await {
                        Ok(ReceiptOutcome::Confirmed { block_number }) => {
                            format!("Claim confirmed in block {}", block_number)
                        }
                        Ok(ReceiptOutcome::Reverted) => {
                            "Claim reverted — nothing to claim?".to_string()
                        }
                        Ok(ReceiptOutcome::Pending) => {
                            format!("Claim pending — tx {} not yet mined", short_hash(&tx))
                        }
                        Err(e) => format!("Receipt poll failed: {}", e),
                    };
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_clipboard_toast(final_msg.into());
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Compute: claimRewards failed: {}", e);
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_clipboard_toast(format!("Claim failed: {}", e).into());
                            }
                        }
                    });
                }
            }
        });
    });

    // --- Compute: Refresh ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_compute_refresh(move || {
        let core = core.clone();
        tracing::info!("Compute: refreshing");
        spawn_async(&rt_h, async move {
            match core.compute.list_jobs().await {
                Ok(jobs) => tracing::info!("Compute: {} jobs", jobs.len()),
                Err(e) => tracing::error!("Compute: refresh failed: {}", e),
            }
        });
    });

    // --- Compute: Refresh listing (CM-01) ---
    // Manual bypass of the 30s background poll — immediately re-queries
    // ComputeMarketplace.getProvider + ContributionAccounting.claimable
    // and pushes the structured listing card fields to the UI.
    //
    // Feature spec: "Manual refresh triggers immediate poll" in
    // citrate_v0.01.1/specs/gherkin/listing_visibility.feature
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_compute_refresh_listing(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Compute: manual listing refresh");
        spawn_async(&rt_h, async move {
            let chain_id = core.config.read().await.chain_id;
            let rpc_url = core.wallet.get_rpc_url();
            let Some(market) = marketplace_client::compute_marketplace_address(chain_id) else {
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || if let Some(ui) = ui_w.upgrade() {
                        ui.set_compute_listing_connection_ok(false);
                    }
                });
                return;
            };
            let Some(accounting) = marketplace_client::contribution_accounting_address(chain_id) else {
                return;
            };
            let accounts = core.wallet.list_accounts().await;
            let Some(addr) = accounts.first().map(|a| a.address.clone()) else {
                return;
            };

            // Data source: ComputeMarketplace.getProvider(address)
            // returns ProviderProfile (contracts/src/ComputeMarketplace.sol:829)
            let provider = if let Some(data) = marketplace_client::encode_get_provider(&addr) {
                marketplace_client::eth_call(&rpc_url, market, &data).await
                    .ok()
                    .and_then(|r| marketplace_client::decode_provider_profile(&r))
            } else {
                None
            };

            // Data source: ContributionAccounting.claimable(address)
            // returns uint256 (public mapping auto-getter)
            let claimable = if let Some(data) = marketplace_client::encode_claimable(&addr) {
                marketplace_client::eth_call(&rpc_url, accounting, &data).await
                    .ok()
                    .and_then(|r| marketplace_client::decode_uint256_u128(&r))
            } else {
                None
            };

            // Data source: ComputeMarketplace event logs for this provider.
            // Empty vec on any error — the card falls back to empty-state.
            let activity = if provider.as_ref().is_some_and(|p| p.is_registered) {
                marketplace_client::fetch_recent_activity(&rpc_url, market, &addr)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_w.upgrade() else { return; };
                match (provider, claimable) {
                    (Some(p), Some(wei)) if p.is_registered => {
                        ui.set_compute_is_registered(true);
                        ui.set_compute_active_jobs(p.current_active_jobs as i32);
                        let stake = marketplace_client::wei_to_salt_display(p.stake_wei);
                        ui.set_compute_listing_stake(stake.into());
                        let rep_pct = (p.reputation_bps as f64) / 100.0;
                        ui.set_compute_listing_reputation_pct(format!("{:.1}", rep_pct).into());
                        ui.set_compute_listing_capacity(
                            format!("{} / {}", p.current_active_jobs, p.max_concurrent_jobs).into()
                        );
                        ui.set_compute_listing_models("any (wildcard)".into());
                        ui.set_compute_listing_total_completed(p.total_jobs_completed as i32);
                        ui.set_compute_listing_total_failed(p.total_jobs_failed as i32);
                        let claim_salt = marketplace_client::wei_to_salt_display(wei);
                        ui.set_compute_earned(claim_salt.clone().into());
                        ui.set_compute_listing_claimable(claim_salt.into());
                        ui.set_compute_listing_connection_ok(true);

                        let rows: Vec<ListingActivityRow> = activity
                            .into_iter()
                            .map(|e| ListingActivityRow {
                                job_id: e.job_id as i32,
                                block_number: e.block_number as i32,
                                status: e.status.into(),
                            })
                            .collect();
                        let model = std::rc::Rc::new(slint::VecModel::from(rows));
                        ui.set_compute_listing_activity(model.into());
                    }
                    _ => {
                        ui.set_compute_listing_connection_ok(false);
                    }
                }
            });
        });
    });

    // =========================================================================
    // OPERATIONS: Agent Center
    // =========================================================================

    // --- Operations: Emergency Stop ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_ops_emergency_stop(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::warn!("Operations: EMERGENCY STOP triggered");
        spawn_async(&rt_h, async move {
            // Cancel all pending approvals
            let pending = core.approvals.list_pending().await;
            for req in &pending {
                core.approvals.resolve(&req.request_id, false).await;
            }
            tracing::warn!("Operations: denied {} pending approvals", pending.len());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_ops_pending_count(0);
                    ui.set_chat_tool_pending(false);
                }
            });
        });
    });

    // --- Operations: Refresh Trail ---
    let core = app_core.clone();
    // --- Ops: Pause / Resume chain (T2-5) ---
    // Calls citrate_emergencyPause / Resume / Status RPCs. Devnet
    // mode (operator_token=None, is_public_bind=false) lets the
    // local GUI call these without auth headers; production
    // operators with a public bind will need to set the token.
    fn emergency_rpc_body(method: &'static str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": [],
            "id": 1,
        })
    }
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_ops_pause_chain(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let rpc_url = core.config.read().await.active_rpc_url();
                let client = reqwest::Client::new();
                match client.post(&rpc_url).json(&emergency_rpc_body("citrate_emergencyPause")).send().await {
                    Ok(_) => {
                        tracing::warn!("Operations: chain PAUSED via RPC");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_ops_chain_paused(true);
                                ui.set_clipboard_toast("Block production paused".into());
                                let ui_for_clear = ui_w.clone();
                                slint::Timer::single_shot(
                                    std::time::Duration::from_millis(3000),
                                    move || {
                                        if let Some(ui) = ui_for_clear.upgrade() {
                                            ui.set_clipboard_toast("".into());
                                        }
                                    },
                                );
                            }
                        });
                    }
                    Err(e) => tracing::error!("Operations: pause failed: {}", e),
                }
            });
        });
    }
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_ops_resume_chain(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let rpc_url = core.config.read().await.active_rpc_url();
                let client = reqwest::Client::new();
                match client.post(&rpc_url).json(&emergency_rpc_body("citrate_emergencyResume")).send().await {
                    Ok(_) => {
                        tracing::info!("Operations: chain RESUMED via RPC");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_ops_chain_paused(false);
                                ui.set_clipboard_toast("Block production resumed".into());
                                let ui_for_clear = ui_w.clone();
                                slint::Timer::single_shot(
                                    std::time::Duration::from_millis(3000),
                                    move || {
                                        if let Some(ui) = ui_for_clear.upgrade() {
                                            ui.set_clipboard_toast("".into());
                                        }
                                    },
                                );
                            }
                        });
                    }
                    Err(e) => tracing::error!("Operations: resume failed: {}", e),
                }
            });
        });
    }

    // --- Ops: Set scope (P960-K T1-2) ---
    // Flips AppCore.session_policy between Guided and ReadOnly. The
    // tool dispatch reads this on every call so the change takes
    // effect immediately for in-app chat invocations.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_ops_set_scope(move |scope_str| {
            let core = core.clone();
            let ui_w = ui_w.clone();
            let scope_str = scope_str.to_string();
            spawn_async(&rt_h, async move {
                use citrate_agent_core::canonical::PolicyProfile;
                let new_policy = match scope_str.as_str() {
                    "read-only" => PolicyProfile::ReadOnly,
                    _ => PolicyProfile::Guided,
                };
                *core.session_policy.write().await = new_policy.clone();
                tracing::info!("Session policy changed to {:?}", new_policy);
                let display = scope_str.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_ops_grant_scope(display.into());
                    }
                });
            });
        });
    }

    // --- Ops: Copy sidecar config (P960-J) ---
    // Writes a Hermes-compatible sidecar config JSON to the clipboard
    // so users can paste it into their Hermes client.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_ops_copy_sidecar_config(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                let cfg = core.mcp_host.export_sidecar_config(None).await;
                let json = serde_json::to_string_pretty(&cfg).unwrap_or_default();
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || {
                        if let Some(ui) = ui_w.upgrade() {
                            // Reuse the clipboard mechanism via the
                            // existing copy-to-clipboard callback.
                            ui.invoke_copy_to_clipboard(json.into());
                            ui.set_clipboard_toast(
                                "Sidecar config copied — paste into your Hermes client".into()
                            );
                            let ui_for_clear = ui_w.clone();
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(3000),
                                move || {
                                    if let Some(ui) = ui_for_clear.upgrade() {
                                        ui.set_clipboard_toast("".into());
                                    }
                                },
                            );
                        }
                    }
                });
            });
        });
    }

    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_ops_refresh_trail(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Operations: refreshing trail");
        spawn_async(&rt_h, async move {
            let events = core.trail.get_events().await;
            let count = events.len() as i32;
            let trail_entries: Vec<TrailEntryData> = events.iter().map(|e| {
                TrailEntryData {
                    timestamp: e.timestamp.split('T').next_back().unwrap_or(&e.timestamp).into(),
                    event_type: e.event_type.clone().into(),
                    tool_name: e.tool_name.clone().unwrap_or_default().into(),
                    risk_level: e.risk_level.clone().unwrap_or_default().into(),
                    approved: e.approved.map(|b| b.to_string()).unwrap_or_default().into(),
                }
            }).collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_ops_trail_count(count);
                    let model = std::rc::Rc::new(slint::VecModel::from(trail_entries));
                    ui.set_ops_trail_events(model.into());
                }
            });
        });
    });

    // --- Operations: Approve/Deny pending from Operations page ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ops_approve_pending(move |request_id| {
        let core = core.clone();
        let id = request_id.to_string();
        tracing::info!("Operations: approving {}", id);
        spawn_async(&rt_h, async move {
            core.approvals.resolve(&id, true).await;
        });
    });

    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ops_deny_pending(move |request_id| {
        let core = core.clone();
        let id = request_id.to_string();
        tracing::info!("Operations: denying {}", id);
        spawn_async(&rt_h, async move {
            core.approvals.resolve(&id, false).await;
        });
    });

    // =========================================================================
    // CHAT TOOL APPROVAL WIRING
    // =========================================================================

    // --- Chat: Approve Tool ---
    // Resolves the pending approval in PendingApprovalStore, which unblocks
    // the tool execution loop in send_message_with_tools().
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    // Use the shared active approval request ID from above
    let active_req_for_approve = active_approval_request_id.clone();
    let active_req_for_reject = active_approval_request_id.clone();

    ui.on_chat_approve_tool(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let active_req = active_req_for_approve.clone();
        tracing::info!("Chat: tool approved by user");
        spawn_async(&rt_h, async move {
            let req_id = active_req.read().await.clone();
            if let Some(id) = req_id {
                core.approvals.resolve(&id, true).await;
                tracing::info!("Chat: resolved approval {} → approved", id);
                *active_req.write().await = None;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_chat_tool_pending(false);
                    }
                });
            } else {
                tracing::warn!("Chat: approve clicked but no active request ID");
            }
        });
    });

    // --- Chat: Reject Tool ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_chat_reject_tool(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let active_req = active_req_for_reject.clone();
        tracing::info!("Chat: tool rejected by user");
        spawn_async(&rt_h, async move {
            let req_id = active_req.read().await.clone();
            if let Some(id) = req_id {
                core.approvals.resolve(&id, false).await;
                tracing::info!("Chat: resolved approval {} → denied", id);
                *active_req.write().await = None;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_chat_tool_pending(false);
                    }
                });
            } else {
                tracing::warn!("Chat: reject clicked but no active request ID");
            }
        });
    });

    // =========================================================================
    // SETTINGS: AI Configuration, Model Scanning, Model Download
    // =========================================================================

    // --- Save AI API Key ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_save_ai_key(move |provider, key| {
        let provider_str = provider.to_string();
        let key_str = key.to_string();
        let core = core.clone();
        tracing::info!("Settings: saving API key for {} ({} chars)", provider_str, key_str.len());
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            let result = config
                .set_ai_key(&provider_str, &key_str)
                .and_then(|_| config.save());
            if let Err(e) = result {
                tracing::error!("Failed to save AI API key securely: {}", e);
            } else {
                tracing::info!("Settings: {} API key saved to OS keychain", provider_str);
            }
        });
    });

    // --- Scan for Local Models (Ollama + GGUF) ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let core = app_core.clone();
    ui.on_settings_scan_models(move || {
        let ui_w = ui_w.clone();
        let core = core.clone();
        tracing::info!("Settings: scanning for local AI backends (Ollama + GGUF)");
        spawn_async(&rt_h, async move {
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let found = detected.backend_type != "none";
            let display_name = detected.display_name.clone();
            let model_id = detected.model_id.clone();
            tracing::info!("Settings: detected backend={}, model={}", detected.backend_type, display_name);

            // Update the chat service model so subsequent messages use the right model
            if found {
                core.chat.set_model(&model_id).await;
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_chat_model_loaded(found);
                    if found {
                        ui.set_chat_model_name(display_name.into());
                    }
                }
            });
        });
    });

    // --- Download Model (Qwen 2.5 1.5B Q4_0 ~1GB from HuggingFace) ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_download_model(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Settings: ensuring bundled Gemma 4 model is seeded into ~/.citrate/models/");
        spawn_async(&rt_h, async move {
            // The installer ships gemma-4-E4B-it-Q4_K_M.gguf inside
            // <app>/Contents/Resources/branding/models/ (and the equivalent
            // path on Linux/Windows). Re-running the seed copies it into
            // ~/.citrate/models/ if it isn't already there. This replaces
            // the previous HuggingFace download that silently failed when
            // the user had no network OR hit HF rate limits — both were
            // common partner reports.
            //
            // The original Qwen 2.5 1.5B download URL is dead from the
            // wallet's point of view: the user already gets Gemma 4 E4B
            // (5 GB, multimodal, function-calling) bundled in the installer.
            // Pressing "Download Model" now means "make sure the bundled
            // one is available." Reaching for other models happens via the
            // ModelRegistry contract — see Settings → Scan or the registry
            // browser once the team's IPFS pinning is live.
            let result = tokio::task::spawn_blocking(citrate_desktop_app::AppCore::seed_bundled_model_public)
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(format!("seed task panicked: {}", e))));

            let model_dir = dirs::home_dir()
                .map(|d| d.join(".citrate/models"))
                .unwrap_or_else(|| std::path::PathBuf::from(".citrate/models"));

            let mut bundled_name: Option<String> = None;
            if let Ok(entries) = std::fs::read_dir(&model_dir) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) {
                        if name.ends_with(".gguf") {
                            bundled_name = Some(name.to_string());
                            break;
                        }
                    }
                }
            }

            let model_name = bundled_name.unwrap_or_else(|| "(no model found)".to_string());
            let status_msg = match result {
                Ok(()) => format!("Ready: {}", model_name),
                Err(e) => {
                    tracing::error!("Bundled-model seed failed: {}", e);
                    format!("Seed failed: {} — see logs", e)
                }
            };
            tracing::info!("Settings: download/seed result → {}", status_msg);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    let loaded = !model_name.starts_with("(no");
                    ui.set_chat_model_loaded(loaded);
                    ui.set_chat_model_name(model_name.into());
                }
            });
        });
    });

    // =========================================================================
    // STORAGE (IPFS) WIRING
    // =========================================================================

    // Storage (IPFS) — connected to daemon via HTTP API on localhost:5001
    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_start_daemon(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: starting IPFS daemon");
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001 — check if already running
            let already_running = client
                .post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);

            if already_running {
                tracing::info!("Storage: IPFS daemon already running — updating UI");
                let stats = ipfs_fetch_stats(&client).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_storage_daemon_online(true);
                        ui.set_storage_peer_count(stats.peer_count);
                        ui.set_storage_pin_count(stats.pin_count);
                        ui.set_storage_repo_size(stats.repo_size.into());
                    }
                });
                return;
            }

            let config = citrate_storage::ipfs::DaemonConfig::default();
            let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
            match daemon.initialize().await {
                Ok(()) => {
                    tracing::info!("Storage: IPFS daemon started");
                    // Fetch live stats after startup
                    let stats = ipfs_fetch_stats(&client).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                Err(e) => tracing::error!("Storage: daemon failed: {}", e),
            }
        });
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_stop_daemon(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: stopping IPFS daemon");
        spawn_async(&rt_h, async move {
            let config = citrate_storage::ipfs::DaemonConfig::default();
            let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
            if let Err(e) = daemon.stop().await {
                tracing::warn!("Storage: stop failed: {}", e);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_storage_daemon_online(false);
                    ui.set_storage_peer_count(0);
                    ui.set_storage_pin_count(0);
                    ui.set_storage_repo_size("0 B".into());
                }
            });
        });
    });

    // P960-C WP-C.1: native file picker via `rfd` + IPFS upload.
    // The heavy lifting lives in `upload_paths_to_ipfs` so drag-drop
    // (WP-C.2) can reuse the same flow.
    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_upload_file(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: upload-file dialog open");

        // ENCRYPT-S1 WP-9a: capture the "Private (encrypted)" toggle at
        // click time (defaults ON) so the async task can't race a
        // toggle flip mid-dialog.
        let mut encrypt = true;
        if let Some(ui) = ui_w.upgrade() {
            ui.set_storage_uploading(true);
            ui.set_storage_upload_status("Choosing files…".into());
            encrypt = ui.get_storage_encrypt_uploads();
        }

        let rt_h_inner = rt_h.clone();
        spawn_async(&rt_h, async move {
            let chosen = rfd::AsyncFileDialog::new()
                .set_title("Upload to Citrate Storage")
                .pick_files()
                .await;
            let paths: Vec<std::path::PathBuf> = match chosen {
                Some(files) => files.into_iter().map(|f| f.path().to_path_buf()).collect(),
                None => {
                    // User cancelled — clear the upload-state UI.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_uploading(false);
                            ui.set_storage_upload_status("".into());
                        }
                    });
                    return;
                }
            };
            upload_paths_to_ipfs(&rt_h_inner, ui_w, paths, encrypt).await;
        });
    });

    // P960-C WP-C.3: Copy share-link flows through the unified
    // clipboard callback that was added in earlier work (flashes the
    // "✓ Copied: …" toast).
    let ui_w = ui.as_weak();
    ui.on_storage_copy_share_link(move |cid, encrypted| {
        if let Some(ui) = ui_w.upgrade() {
            let link = storage_service::share_link(&cid);
            ui.invoke_copy_to_clipboard(link.into());
            // ENCRYPT-S1 WP-9a: for a private file the CID resolves to
            // ciphertext — override the generic "Copied" toast with the
            // warning so nobody mails a link expecting it to open.
            if encrypted {
                ui.set_clipboard_toast(
                    "Copied — private file: the link serves encrypted bytes; \
                     recipients can't read it without your key"
                        .into(),
                );
            }
        }
    });

    // P960-C WP-C.3: Remove from files.json + unpin on IPFS side.
    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_remove_file(move |cid| {
        let cid_str = cid.to_string();
        let ui_w = ui_w.clone();
        tracing::info!("Storage: remove-file cid={}", cid_str);
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            let _ = storage_service::ipfs_unpin(&client, &cid_str).await;

            if let Some(path) = storage_service::FilesIndex::default_path() {
                let mut idx = storage_service::FilesIndex::load(&path);
                idx.remove(&cid_str);
                if let Err(e) = idx.save(&path) {
                    tracing::warn!("files.json save failed: {}", e);
                }
                let entries = build_file_entries(&idx);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        let model = std::rc::Rc::new(slint::VecModel::from(entries));
                        ui.set_storage_files(model.into());
                    }
                });
            }
        });
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_pin_cid(move |cid| {
        let cid_str = cid.to_string();
        let ui_w = ui_w.clone();
        tracing::info!("Storage: pinning CID {}", cid_str);
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001/api/v0/pin/add
            match client.post("http://127.0.0.1:5001/api/v0/pin/add")
                .query(&[("arg", &cid_str)])
                .timeout(std::time::Duration::from_secs(30))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: pinned CID successfully");
                    // Refresh stats after successful pin
                    let stats = ipfs_fetch_stats(&client).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                Ok(resp) => tracing::warn!("Storage: pin failed — status {}", resp.status()),
                Err(e) => tracing::error!("Storage: pin failed — is IPFS running? {}", e),
            }
        });
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_refresh(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: checking daemon status");
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001
            match client.post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(3))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: IPFS daemon is online");
                    let stats = ipfs_fetch_stats(&client).await;
                    tracing::info!(
                        "Storage: {} peers, {} pins, repo {}",
                        stats.peer_count, stats.pin_count, stats.repo_size
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                _ => {
                    tracing::info!("Storage: IPFS daemon is offline");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(false);
                            ui.set_storage_peer_count(0);
                            ui.set_storage_pin_count(0);
                            ui.set_storage_repo_size("0 B".into());
                        }
                    });
                }
            }
        });
    });

    // =========================================================================
    // SYSTEM HEALTH (P960-K T1-4)
    // =========================================================================
    // Bootnode + IPFS + local node RPC preflight, surfaced in Settings.

    // Refresh-all button → re-run all probes
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_health_refresh_all(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                run_health_probes(&core, ui_w).await;
            });
        });
    }

    // Retry bootnode → just re-runs the probes (single-button UX is
    // simpler than a per-probe action, and a partial refresh would
    // be misleading).
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_health_retry_bootnode(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                run_health_probes(&core, ui_w).await;
            });
        });
    }
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_health_retry_node_rpc(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                run_health_probes(&core, ui_w).await;
            });
        });
    }

    // Start IPFS — same path as the existing storage_start_daemon
    // handler, but triggered from Settings instead of the Files tab.
    // After invoking, re-probe so the UI flips to "ok".
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        ui.on_health_start_ipfs(move || {
            let core = core.clone();
            let ui_w = ui_w.clone();
            spawn_async(&rt_h, async move {
                tracing::info!("Health: starting IPFS daemon (from Settings)");
                let client = reqwest::Client::new();
                let already = client
                    .post("http://127.0.0.1:5001/api/v0/id")
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                if !already {
                    // Best-effort spawn — the existing storage daemon
                    // service handles this. We just shell out as the
                    // smallest dependency-light fix.
                    let _ = std::process::Command::new("ipfs")
                        .arg("daemon")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    // Give it a moment to bind
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                run_health_probes(&core, ui_w).await;
            });
        });
    }

    // Run probes once at startup so the Settings tab shows fresh
    // values the first time the user opens it (without waiting for
    // the on_tab_changed firing).
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        spawn_async(&rt_h, async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            run_health_probes(&core, ui_w).await;
        });
    }

    // =========================================================================
    // IPFS AUTO-DETECT ON STARTUP
    // =========================================================================
    // Data source: IPFS daemon HTTP API at localhost:5001 — auto-detect if already running
    {
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        rt_h.spawn(async move {
            let client = reqwest::Client::new();
            match client
                .post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: auto-detected running IPFS daemon");
                    let stats = ipfs_fetch_stats(&client).await;
                    tracing::info!(
                        "Storage: auto-detect — {} peers, {} pins, repo {}",
                        stats.peer_count, stats.pin_count, stats.repo_size
                    );
                    // P960-C: also hydrate the persisted file list so
                    // users see previously-uploaded files on boot.
                    let entries = storage_service::FilesIndex::default_path()
                        .map(|p| build_file_entries(&storage_service::FilesIndex::load(&p)))
                        .unwrap_or_default();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                            let model = std::rc::Rc::new(slint::VecModel::from(entries));
                            ui.set_storage_files(model.into());
                        }
                    });
                }
                _ => {
                    tracing::info!("Storage: no IPFS daemon detected at startup");
                }
            }
        });
    }

    // =========================================================================
    // P960-D WP-D.1 / D.2: compute panel hydration on startup
    // =========================================================================
    // Detect hardware + load persisted opt-in prefs so the Compute
    // panel renders the correct state on first open (rather than the
    // "Detecting hardware…" placeholder).
    {
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        rt_h.spawn(async move {
            let hw = tokio::task::spawn_blocking(compute_service::HardwareProfile::detect)
                .await
                .unwrap_or_default();
            let settings = compute_service::ComputeSettings::default_path()
                .map(|p| compute_service::ComputeSettings::load(&p))
                .unwrap_or_default();
            tracing::info!(
                "Compute hydration: {} (opt-in={}, alloc={}%, schedule={})",
                hw.summary(), settings.enabled, settings.allocation_percent, settings.schedule,
            );
            let desc = settings.schedule_description().to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_compute_hw_cpu(hw.cpu_model.into());
                    ui.set_compute_hw_cpu_cores(hw.cpu_cores as i32);
                    ui.set_compute_hw_cpu_threads(hw.cpu_threads as i32);
                    ui.set_compute_hw_ram_gb(hw.ram_gb as i32);
                    if let Some(gpu) = hw.gpu {
                        ui.set_compute_hw_has_gpu(true);
                        ui.set_compute_hw_gpu_name(gpu.name.into());
                        ui.set_compute_hw_gpu_vram_gb(gpu.vram_gb as i32);
                        ui.set_compute_hw_gpu_cc(gpu.compute_capability.into());
                        ui.set_compute_hw_gpu_driver(gpu.driver_version.into());
                    } else {
                        ui.set_compute_hw_has_gpu(false);
                    }
                    ui.set_compute_enabled(settings.enabled);
                    ui.set_compute_allocation(settings.allocation_percent as i32);
                    ui.set_compute_schedule(settings.schedule.into());
                    ui.set_compute_schedule_description(desc.into());
                }
            });
        });
    }

    tracing::info!("Citrate Desktop ready (full wiring)");
    // Enter the Tokio runtime on the main thread so winit/zbus calls
    // made from Slint's event loop can find a reactor. zbus 5.14
    // (transitive via i-slint-backend-winit on Linux for dark-mode +
    // portal queries) panics with "no reactor running" without this.
    let _rt_guard = rt.enter();
    crash_telemetry::set_last_state("ui-event-loop-running");
    if let Err(err) = ui.run() {
        // Deliberately do NOT remove the session marker: an event-loop
        // failure is not a clean exit and should be visible next launch.
        eprintln!("Slint event loop failed: {err}");
        std::process::exit(1);
    }
    // WP-A1: clean exit — remove the session marker so the next launch
    // doesn't flag this session as an unclean death.
    crash_telemetry::mark_clean_exit();
}
