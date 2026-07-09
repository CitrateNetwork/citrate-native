//! Property-based (fuzz-style) tests using proptest.
//!
//! These tests generate random inputs and verify invariants hold for ALL inputs,
//! not just hand-picked examples. This catches edge cases that unit tests miss.

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::services::wallet_service::{
    WalletService, WalletBackend, Account, CreateAccountResult,
};
use citrate_desktop_app::{AppConfig, AppCore};
use proptest::prelude::*;
use std::sync::Arc;

/// Local test wallet backend for property tests.
struct LocalTestWalletBackend;

#[async_trait::async_trait]
impl WalletBackend for LocalTestWalletBackend {
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(Vec::new()) }
    async fn create_wallet(&self, _password: &str, _label: &str) -> Result<CreateAccountResult, AppError> {
        Ok(CreateAccountResult {
            address: "0x0000000000000000000000000000000000000000".to_string(),
            mnemonic: "test words".to_string(),
            public_key: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        })
    }
    async fn unlock(&self, _address: &str, password: &str) -> Result<bool, AppError> {
        Ok(password.len() >= 8)
    }
    async fn lock(&self) -> Result<(), AppError> { Ok(()) }
    async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
        Ok("0x0000000000000000000000000000000000000000000000000000000000000000".to_string())
    }
}

// =========================================================================
// ADDRESS VALIDATION PROPERTIES
// =========================================================================

proptest! {
    /// Any string that is not exactly 40 hex chars (with optional 0x prefix) must be rejected.
    #[test]
    fn prop_invalid_length_addresses_rejected(s in "[a-fA-F0-9]{0,39}|[a-fA-F0-9]{41,100}") {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let core = AppCore::new();
            let result = core.node.get_balance(&s).await;
            prop_assert!(result.is_err(), "Address '{}' should be rejected (wrong length)", s);
            Ok(())
        })?;
    }

    /// Valid 40-hex-char addresses must always be accepted.
    #[test]
    fn prop_valid_hex_addresses_accepted(s in "[a-fA-F0-9]{40}") {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let core = AppCore::new();
            let result = core.node.get_balance(&format!("0x{}", s)).await;
            prop_assert!(result.is_ok(), "Valid hex address '0x{}' should be accepted", s);
            Ok(())
        })?;
    }

    /// Non-hex characters in address must be rejected.
    #[test]
    fn prop_non_hex_addresses_rejected(s in "[g-zG-Z!@#$%^&*()]{40}") {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let core = AppCore::new();
            let result = core.node.get_balance(&format!("0x{}", s)).await;
            prop_assert!(result.is_err(), "Non-hex address '0x{}' should be rejected", s);
            Ok(())
        })?;
    }
}

// =========================================================================
// PASSWORD VALIDATION PROPERTIES
// =========================================================================

proptest! {
    /// Passwords with fewer than 8 bytes must always be rejected.
    /// Uses ASCII-only to ensure byte length == character count.
    #[test]
    fn prop_short_passwords_rejected(s in "[a-zA-Z0-9!@#$%^&*]{0,7}") {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let events = Arc::new(EventBus::new());
            let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
            let result: Result<_, _> = svc.create_wallet(&s).await;
            prop_assert!(result.is_err(), "Password '{}' (len {}) should be rejected", s, s.len());
            Ok(())
        })?;
    }

    /// Passwords with 8+ bytes must always be accepted.
    /// The validation checks byte length (.len()), not char count.
    #[test]
    fn prop_long_passwords_accepted(s in "[a-zA-Z0-9!@#$%^&*]{8,100}") {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let events = Arc::new(EventBus::new());
            let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
            let result: Result<_, _> = svc.create_wallet(&s).await;
            prop_assert!(result.is_ok(), "Password (len {}) should be accepted", s.len());
            Ok(())
        })?;
    }

    /// Multibyte Unicode passwords: byte length >= 8 must pass even if char count < 8.
    /// This verifies that password validation correctly uses byte length for entropy.
    #[test]
    fn prop_unicode_password_byte_length_matters(s in ".{1,4}") {
        // If the string's byte length is >= 8 despite few characters, it should pass
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let events = Arc::new(EventBus::new());
            let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
            let result: Result<_, _> = svc.create_wallet(&s).await;
            if s.len() >= 8 {
                prop_assert!(result.is_ok(), "Byte len {} >= 8 should pass", s.len());
            } else {
                prop_assert!(result.is_err(), "Byte len {} < 8 should fail", s.len());
            }
            Ok(())
        })?;
    }
}

// =========================================================================
// CONFIG SERIALIZATION PROPERTIES
// =========================================================================

proptest! {
    /// Config roundtrip: serialize → deserialize must preserve all fields.
    #[test]
    fn prop_config_roundtrip(
        network in "[a-z]{3,10}",
        chain_id in 1u64..u64::MAX,
        rpc_port in 1024u16..65535u16,
        p2p_port in 1024u16..65535u16,
        theme in "(dark|light)",
    ) {
        let config = AppConfig {
            network: network.clone(),
            chain_id,
            data_dir: "/tmp/test".to_string(),
            rpc_port,
            p2p_port,
            mcp_port: 0,
            bootnodes: vec![],
            theme: theme.clone(),
            ai_keys: std::collections::HashMap::new(),
            ai_priority: vec!["local".to_string()],
            logseq_graph_path: "/tmp/logseq".to_string(),
            logseq_enabled: false,
            auto_journal: false,
            on_chain_anchoring: false,
            integration_tokens: std::collections::HashMap::new(),
            encryption_at_rest: true,
        };

        let json = serde_json::to_string(&config).expect("serialization succeeded");
        let deser: AppConfig = serde_json::from_str(&json).expect("test assertion");

        prop_assert_eq!(deser.network, network);
        prop_assert_eq!(deser.chain_id, chain_id);
        prop_assert_eq!(deser.rpc_port, rpc_port);
        prop_assert_eq!(deser.p2p_port, p2p_port);
        prop_assert_eq!(deser.theme, theme);
    }
}

// =========================================================================
// EVENT BUS PROPERTIES
// =========================================================================

proptest! {
    /// Publishing N events should result in subscriber receiving exactly N events.
    #[test]
    fn prop_event_bus_delivers_all(count in 1usize..50) {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();

            for i in 0..count {
                bus.publish(AppEvent::NodeStatusChanged {
                    running: true,
                    block_height: i as u64,
                    peer_count: 0,
                    syncing: false,
                });
            }

            let mut received = 0;
            for _ in 0..count {
                if rx.recv().await.is_ok() {
                    received += 1;
                }
            }
            prop_assert_eq!(received, count);
            Ok(())
        })?;
    }
}

// =========================================================================
// ERROR DISPLAY PROPERTIES
// =========================================================================

proptest! {
    /// Error Display must never panic and always produce non-empty output.
    #[test]
    fn prop_error_display_never_panics(msg in ".*") {
        let errors: Vec<AppError> = vec![
            AppError::Node(msg.clone()),
            AppError::Wallet(msg.clone()),
            AppError::Storage(msg.clone()),
            AppError::Network(msg.clone()),
            AppError::Config(msg.clone()),
            AppError::ChainQuery(msg.clone()),
            AppError::InvalidAddress(msg.clone()),
            AppError::RateLimited(msg.clone()),
            AppError::ModelNotLoaded(msg.clone()),
        ];
        for err in errors {
            let display = err.to_string();
            prop_assert!(!display.is_empty(), "Error Display must not be empty");
        }
    }
}

// =========================================================================
// WALLET SERVICE — STATE MACHINE PROPERTIES
// =========================================================================

proptest! {
    /// After N unlock/lock cycles, session state must be consistent.
    #[test]
    fn prop_lock_unlock_state_consistent(cycles in 1usize..20) {
        let rt = tokio::runtime::Runtime::new().expect("test assertion");
        rt.block_on(async {
            let events = Arc::new(EventBus::new());
            let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));

            for _ in 0..cycles {
                let _: Result<_, _> = svc.unlock("0x", "password123").await;
                let s = svc.get_session_status().await;
                prop_assert!(s.is_active, "Session must be active after unlock");

                let _: Result<(), _> = svc.lock().await;
                let s = svc.get_session_status().await;
                prop_assert!(!s.is_active, "Session must be inactive after lock");
            }
            Ok(())
        })?;
    }
}
