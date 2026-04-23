//! Security, edge case, and adversarial input tests.
//!
//! These tests simulate malicious or unexpected inputs that a production
//! wallet GUI would encounter. They cover:
//! - Address injection attacks
//! - Unicode/emoji in password fields
//! - Integer overflow in amounts
//! - Null bytes and control characters
//! - Concurrent access patterns
//! - Resource exhaustion scenarios
//! - Session bypass attempts

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::services::wallet_service::{
    Account, CreateAccountResult, WalletBackend, WalletService,
};
use citrate_desktop_app::services::node_service::{
    NodeBackend, NodeService, NodeStatus,
};
use citrate_desktop_app::{AppConfig, AppCore};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Local test node backend for integration tests.
/// Avoids RocksDB and port binding in parallel test environments.
struct LocalTestNodeBackend;

#[async_trait::async_trait]
impl NodeBackend for LocalTestNodeBackend {
    async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> { Ok(()) }
    async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
    async fn get_block_height(&self) -> u64 { 0 }
    async fn get_peer_count(&self) -> u32 { 0 }
    async fn get_mempool_size(&self) -> usize { 0 }
    async fn get_balance(&self, _: &[u8; 20]) -> String { "0".to_string() }
}

/// Helper: create an AppCore with test node backend (no real storage/networking)
fn test_app_core() -> AppCore {
    let config = Arc::new(RwLock::new(AppConfig::default()));
    let events = Arc::new(EventBus::new());
    let node = Arc::new(NodeService::with_backend(
        config.clone(), events.clone(), Arc::new(LocalTestNodeBackend),
    ));
    let wallet = Arc::new(citrate_desktop_app::services::WalletService::new(events.clone()));
    let chat = Arc::new(citrate_desktop_app::services::ChatService::new(events.clone(), "https://rpc.citrate.ai"));
    let models = Arc::new(citrate_desktop_app::services::ModelService::new(events.clone(), "https://rpc.citrate.ai"));
    let blocks = Arc::new(citrate_desktop_app::services::BlockService::new(events.clone(), "https://rpc.citrate.ai"));
    let learning = Arc::new(citrate_desktop_app::services::LearningService::new(events.clone(), "https://rpc.citrate.ai"));
    let compute = Arc::new(citrate_desktop_app::services::ComputeService::new(events.clone(), "https://rpc.citrate.ai"));
    let trail = Arc::new(citrate_desktop_app::trail::TrailRecorder::new("test-session", None));
    let approvals = Arc::new(citrate_agent_core::delegation::PendingApprovalStore::new());
    let tool_registry = Arc::new(citrate_agent_core::tool::ToolRegistry::new());
    AppCore { node, wallet, chat, models, blocks, learning, compute, events, trail, approvals, tool_registry, config }
}

/// Local test wallet backend for integration tests.
/// Mirrors the #[cfg(test)] TestWalletBackend but available to integration test crate.
struct LocalTestWalletBackend;

#[async_trait::async_trait]
impl WalletBackend for LocalTestWalletBackend {
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(Vec::new()) }
    async fn create_wallet(&self, _password: &str, _label: &str) -> Result<CreateAccountResult, AppError> {
        Ok(CreateAccountResult {
            address: "0x0000000000000000000000000000000000000000".to_string(),
            mnemonic: "test mnemonic words for development only not real".to_string(),
            public_key: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        })
    }
    async fn unlock(&self, _address: &str, password: &str) -> Result<bool, AppError> {
        Ok(password.len() >= 8)
    }
    async fn lock(&self) -> Result<(), AppError> { Ok(()) }
    async fn send_transaction(&self, _from: &str, _to: &str, _value: &str, _password: &str) -> Result<String, AppError> {
        Ok("0x0000000000000000000000000000000000000000000000000000000000000000".to_string())
    }
}

// =========================================================================
// ADDRESS VALIDATION — INJECTION & MALFORMED INPUTS
// =========================================================================

#[tokio::test]
async fn test_address_with_sql_injection() {
    let core = AppCore::new();
    let result = core.node.get_balance("0x' OR 1=1; DROP TABLE --").await;
    assert!(result.is_err(), "SQL injection string must be rejected");
}

#[tokio::test]
async fn test_address_with_html_injection() {
    let core = AppCore::new();
    let result = core.node.get_balance("<script>alert('xss')</script>").await;
    assert!(result.is_err(), "HTML injection must be rejected");
}

#[tokio::test]
async fn test_address_with_null_bytes() {
    let core = AppCore::new();
    let result = core.node.get_balance("0x0000\x00000000000000000000000000000000000000").await;
    assert!(result.is_err(), "Null bytes in address must be rejected");
}

#[tokio::test]
async fn test_address_with_unicode() {
    let core = AppCore::new();
    let result = core.node.get_balance("0x日本語アドレス").await;
    assert!(result.is_err(), "Unicode in hex address must be rejected");
}

#[tokio::test]
async fn test_address_with_newlines() {
    let core = AppCore::new();
    let result = core.node.get_balance("0xb5ddd4eb356ddf3b\nf51eb3aec1ed28213be59129").await;
    assert!(result.is_err(), "Newlines in address must be rejected");
}

#[tokio::test]
async fn test_address_all_zeros() {
    let core = AppCore::new();
    let result = core.node.get_balance("0x0000000000000000000000000000000000000000").await;
    assert!(result.is_ok(), "Zero address should be valid hex");
}

#[tokio::test]
async fn test_address_all_ff() {
    let core = AppCore::new();
    let result = core.node.get_balance("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF").await;
    assert!(result.is_ok(), "All-F address should be valid hex");
}

#[tokio::test]
async fn test_address_mixed_case() {
    let core = AppCore::new();
    let result = core.node.get_balance("0xB5dDd4eB356dDf3Bf51eB3AeC1eD28213bE59129").await;
    assert!(result.is_ok(), "Mixed case hex should be accepted");
}

#[tokio::test]
async fn test_address_only_0x_prefix() {
    let core = AppCore::new();
    let result = core.node.get_balance("0x").await;
    assert!(result.is_err(), "Bare 0x prefix must be rejected");
}

#[tokio::test]
async fn test_address_extremely_long() {
    let core = AppCore::new();
    let long_addr = format!("0x{}", "a".repeat(1000));
    let result = core.node.get_balance(&long_addr).await;
    assert!(result.is_err(), "1000-char address must be rejected");
}

// =========================================================================
// PASSWORD SECURITY — EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_password_with_null_bytes() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    let result = svc.create_wallet("pass\x00word1234").await;
    // Should succeed — null bytes in password are the user's problem,
    // but the system must not crash
    assert!(result.is_ok(), "Null bytes in password should not crash");
}

#[tokio::test]
async fn test_password_with_unicode() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    let result = svc.create_wallet("パスワード12345678").await;
    assert!(result.is_ok(), "Unicode password should be accepted");
}

#[tokio::test]
async fn test_password_with_emoji() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    let result = svc.create_wallet("🔑🔐🔒🔓🔏🔎🔍🗝️").await;
    assert!(result.is_ok(), "Emoji password should be accepted (8+ chars)");
}

#[tokio::test]
async fn test_password_very_long() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    let long_pass = "a".repeat(10_000);
    let result = svc.create_wallet(&long_pass).await;
    assert!(result.is_ok(), "Very long password should be accepted");
}

#[tokio::test]
async fn test_password_all_spaces() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    let result = svc.create_wallet("        ").await; // 8 spaces
    assert!(result.is_ok(), "8 spaces meets length requirement");
}

#[tokio::test]
async fn test_password_boundary_7_chars() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    assert!(svc.create_wallet("1234567").await.is_err());
}

#[tokio::test]
async fn test_password_boundary_8_chars() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    assert!(svc.create_wallet("12345678").await.is_ok());
}

#[tokio::test]
async fn test_password_boundary_9_chars() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    assert!(svc.create_wallet("123456789").await.is_ok());
}

// =========================================================================
// SESSION BYPASS ATTEMPTS
// =========================================================================

#[tokio::test]
async fn test_send_without_session_is_blocked() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    // Never unlocked — session is not active
    let result = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await;
    match result {
        Err(AppError::SessionExpired) => {}
        other => panic!("Expected SessionExpired, got {:?}", other),
    }
}

#[tokio::test]
async fn test_send_after_lock_is_blocked() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    svc.lock().await.expect("lock succeeded");
    let result = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await;
    match result {
        Err(AppError::SessionExpired) => {}
        other => panic!("Expected SessionExpired after lock, got {:?}", other),
    }
}

#[tokio::test]
async fn test_rapid_lock_unlock_cycles() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    for _ in 0..100 {
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        assert!(svc.get_session_status().await.is_active);
        svc.lock().await.expect("lock succeeded");
        assert!(!svc.get_session_status().await.is_active);
    }
}

#[tokio::test]
async fn test_double_lock_is_safe() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    svc.lock().await.expect("lock succeeded");
    svc.lock().await.expect("lock succeeded"); // double lock should not panic
    assert!(!svc.get_session_status().await.is_active);
}

#[tokio::test]
async fn test_double_unlock_is_safe() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    assert!(svc.get_session_status().await.is_active);
}

// =========================================================================
// CONCURRENT ACCESS
// =========================================================================

#[tokio::test]
async fn test_concurrent_event_publish() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let mut handles = vec![];
    for i in 0..50 {
        let bus = bus.clone();
        handles.push(tokio::spawn(async move {
            bus.publish(AppEvent::NodeStatusChanged {
                running: true,
                block_height: i,
                peer_count: 0,
                syncing: false,
            });
        }));
    }

    for h in handles {
        h.await.expect("async operation succeeded");
    }

    let mut count = 0;
    while let Ok(Ok(_)) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await {
        count += 1;
    }
    assert_eq!(count, 50, "All 50 concurrent events should be received");
}

#[tokio::test]
async fn test_concurrent_status_reads() {
    let core = test_app_core();
    core.start().await.expect("start succeeded");

    let mut handles = vec![];
    let node = core.node.clone();
    for _ in 0..20 {
        let node = node.clone();
        handles.push(tokio::spawn(async move {
            let status = node.get_status().await;
            assert!(status.running);
            assert_eq!(status.chain_id, 40204);
        }));
    }

    for h in handles {
        h.await.expect("async operation succeeded");
    }
}

#[tokio::test]
async fn test_concurrent_balance_queries() {
    let core = test_app_core();
    let mut handles = vec![];
    let node = core.node.clone();
    for _ in 0..20 {
        let node = node.clone();
        handles.push(tokio::spawn(async move {
            let result = node.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await;
            assert!(result.is_ok());
        }));
    }
    for h in handles {
        h.await.expect("async operation succeeded");
    }
}

// =========================================================================
// TRANSACTION AMOUNT EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_send_zero_amount() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    let result = svc.send_transaction("0xfrom", "0xto", "0", "pwd").await;
    // Zero amount should be allowed (gas-only tx)
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_send_max_u256_amount() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    let max_u256 = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let result = svc.send_transaction("0xfrom", "0xto", max_u256, "pwd").await;
    assert!(result.is_ok(), "Max U256 should not crash the system");
}

#[tokio::test]
async fn test_send_negative_amount() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    let result = svc.send_transaction("0xfrom", "0xto", "-1000", "pwd").await;
    // StubBackend doesn't validate, but the system must not crash
    assert!(result.is_ok(), "Backend should handle negative gracefully");
}

#[tokio::test]
async fn test_send_non_numeric_amount() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    let result = svc.send_transaction("0xfrom", "0xto", "not_a_number", "pwd").await;
    assert!(result.is_ok(), "StubBackend passes — real backend should validate");
}

#[tokio::test]
async fn test_send_empty_amount() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));
    svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
    let result = svc.send_transaction("0xfrom", "0xto", "", "pwd").await;
    assert!(result.is_ok(), "Empty amount passes through to backend");
}

// =========================================================================
// NODE LIFECYCLE EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_node_start_stop_rapid_cycling() {
    let core = test_app_core();
    for _ in 0..20 {
        core.node.start().await.expect("start succeeded");
        core.node.stop().await.expect("stop succeeded");
    }
    assert!(!core.node.get_status().await.running);
}

#[tokio::test]
async fn test_app_core_double_start() {
    let core = test_app_core();
    core.start().await.expect("start succeeded");
    core.start().await.expect("start succeeded");
    assert!(core.node.get_status().await.running);
}

#[tokio::test]
async fn test_app_core_stop_without_start() {
    let core = test_app_core();
    core.stop().await.expect("stop succeeded");
    assert!(!core.node.get_status().await.running);
}

// =========================================================================
// CONFIG EDGE CASES
// =========================================================================

#[test]
fn test_config_invalid_json() {
    let result = serde_json::from_str::<AppConfig>("not json");
    assert!(result.is_err());
}

#[test]
fn test_config_partial_json() {
    let result = serde_json::from_str::<AppConfig>(r#"{"network":"test"}"#);
    assert!(result.is_err(), "Missing required fields should fail");
}

#[test]
fn test_config_extra_fields_ignored() {
    let json = r#"{"network":"devnet","chain_id":40204,"data_dir":"/tmp","rpc_port":18545,"p2p_port":30304,"bootnodes":[],"theme":"dark","extra_field":"ignored"}"#;
    let result = serde_json::from_str::<AppConfig>(json);
    assert!(result.is_ok(), "Extra fields should be silently ignored");
}

#[test]
fn test_config_chain_id_zero() {
    let json = r#"{"network":"test","chain_id":0,"data_dir":"/tmp","rpc_port":18545,"p2p_port":30304,"bootnodes":[],"theme":"dark"}"#;
    let config: AppConfig = serde_json::from_str(json).expect("deserialization succeeded");
    assert_eq!(config.chain_id, 0);
}

#[test]
fn test_config_chain_id_max() {
    let json = r#"{"network":"test","chain_id":18446744073709551615,"data_dir":"/tmp","rpc_port":18545,"p2p_port":30304,"bootnodes":[],"theme":"dark"}"#;
    let config: AppConfig = serde_json::from_str(json).expect("deserialization succeeded");
    assert_eq!(config.chain_id, u64::MAX);
}

#[test]
fn test_config_empty_bootnodes() {
    let json = r#"{"network":"test","chain_id":40204,"data_dir":"/tmp","rpc_port":18545,"p2p_port":30304,"bootnodes":[],"theme":"dark"}"#;
    let config: AppConfig = serde_json::from_str(json).expect("deserialization succeeded");
    assert!(config.bootnodes.is_empty());
}

#[test]
fn test_config_many_bootnodes() {
    let nodes: Vec<String> = (0..100).map(|i| format!("node{}@1.2.3.4:{}", i, 30000 + i)).collect();
    let config = AppConfig {
        bootnodes: nodes.clone(),
        ..AppConfig::default()
    };
    assert_eq!(config.bootnodes.len(), 100);
    let json = serde_json::to_string(&config).expect("serialization succeeded");
    let deser: AppConfig = serde_json::from_str(&json).expect("test assertion");
    assert_eq!(deser.bootnodes.len(), 100);
}

// =========================================================================
// ERROR PROPAGATION — BACKEND FAILURES
// =========================================================================

#[tokio::test]
async fn test_backend_start_failure_propagates() {
    struct FailStart;

    #[async_trait::async_trait]
    impl NodeBackend for FailStart {
        async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> {
            Err(AppError::Node("port already in use".into()))
        }
        async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
        async fn get_block_height(&self) -> u64 { 0 }
        async fn get_peer_count(&self) -> u32 { 0 }
        async fn get_mempool_size(&self) -> usize { 0 }
        async fn get_balance(&self, _: &[u8; 20]) -> String { "0".into() }
    }

    let config = Arc::new(RwLock::new(AppConfig::default()));
    let events = Arc::new(EventBus::new());
    let svc = NodeService::with_backend(config, events, Arc::new(FailStart));

    let err = svc.start().await.expect_err("expected error");
    match err {
        AppError::Node(msg) => assert!(msg.contains("port already in use")),
        other => panic!("Expected Node error, got {:?}", other),
    }
    // Node should NOT be marked as running after failed start
    assert!(!svc.get_status().await.running);
}

#[tokio::test]
async fn test_backend_stop_failure_propagates() {
    struct FailStop;

    #[async_trait::async_trait]
    impl NodeBackend for FailStop {
        async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> { Ok(()) }
        async fn stop_node(&self) -> Result<(), AppError> {
            Err(AppError::Node("process not responding".into()))
        }
        async fn get_block_height(&self) -> u64 { 0 }
        async fn get_peer_count(&self) -> u32 { 0 }
        async fn get_mempool_size(&self) -> usize { 0 }
        async fn get_balance(&self, _: &[u8; 20]) -> String { "0".into() }
    }

    let config = Arc::new(RwLock::new(AppConfig::default()));
    let events = Arc::new(EventBus::new());
    let svc = NodeService::with_backend(config, events, Arc::new(FailStop));
    svc.start().await.expect("start succeeded");

    let err = svc.stop().await.expect_err("expected error");
    match err {
        AppError::Node(msg) => assert!(msg.contains("not responding")),
        other => panic!("Expected Node error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_wallet_backend_unlock_error_propagates() {
    struct ErrorUnlock;

    #[async_trait::async_trait]
    impl WalletBackend for ErrorUnlock {
        async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(vec![]) }
        async fn create_wallet(&self, _: &str, _: &str) -> Result<CreateAccountResult, AppError> {
            Ok(CreateAccountResult { address: "0x".into(), mnemonic: "w".into(), public_key: "k".into() })
        }
        async fn unlock(&self, _: &str, _: &str) -> Result<bool, AppError> {
            Err(AppError::Storage("keystore corrupted".into()))
        }
        async fn lock(&self) -> Result<(), AppError> { Ok(()) }
        async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
            Ok("0x".into())
        }
    }

    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(ErrorUnlock));
    let err = svc.unlock("0xabc", "password123").await.expect_err("expected error");
    match err {
        AppError::Storage(msg) => assert!(msg.contains("corrupted")),
        other => panic!("Expected Storage error, got {:?}", other),
    }
}

// =========================================================================
// INTEGRATION — CROSS-SERVICE INTERACTION
// =========================================================================

#[tokio::test]
async fn test_full_lifecycle_create_unlock_send_lock() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));

    // Create wallet
    let result = svc.create_wallet("strongpassword123").await.expect("create wallet");
    assert!(!result.address.is_empty());

    // Verify not first run
    assert!(!svc.is_first_run().await);

    // Unlock
    let status = svc.unlock("default", "password123").await.expect("unlock");
    assert!(status.is_active);

    // Send
    let tx = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await.expect("send");
    assert!(!tx.is_empty());

    // Lock
    svc.lock().await.expect("lock succeeded");
    assert!(!svc.get_session_status().await.is_active);

    // Send after lock fails
    let fail = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await;
    assert!(matches!(fail, Err(AppError::SessionExpired)));
}

#[tokio::test]
async fn test_events_flow_during_lifecycle() {
    let events = Arc::new(EventBus::new());
    let mut rx = events.subscribe();
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));

    svc.unlock("0x", "password123").await.expect("unlock");
    svc.send_transaction("0xfrom", "0xto", "1", "p").await.expect("send");

    let event = rx.recv().await.expect("event received");
    assert!(matches!(event, AppEvent::TransactionConfirmed { success: true, .. }));
}

#[tokio::test]
async fn test_multiple_wallets_independent() {
    let events = Arc::new(EventBus::new());
    let svc = WalletService::with_backend(events, Arc::new(LocalTestWalletBackend));

    svc.create_wallet("strongpassword1!").await.expect("async operation succeeded");
    let accounts1 = svc.list_accounts().await;
    assert_eq!(accounts1.len(), 1);

    // Creating another wallet adds to the list
    svc.create_wallet("strongpassword2!").await.expect("async operation succeeded");
    let accounts2 = svc.list_accounts().await;
    assert_eq!(accounts2.len(), 2);
}

// =========================================================================
// NODE STATUS — DATA INTEGRITY
// =========================================================================

#[tokio::test]
async fn test_node_status_fields_are_consistent() {
    let core = test_app_core();
    let status = core.node.get_status().await;
    // Stopped node should have zero for all metrics
    assert!(!status.running);
    assert_eq!(status.block_height, 0);
    assert_eq!(status.peer_count, 0);
    assert_eq!(status.mempool_size, 0);

    core.start().await.expect("start succeeded");
    let status = core.node.get_status().await;
    assert!(status.running);
    assert_eq!(status.chain_id, 40204);
}

#[test]
fn test_node_status_default_all_zero() {
    let status = NodeStatus::default();
    assert!(!status.running);
    assert_eq!(status.block_height, 0);
    assert_eq!(status.peer_count, 0);
    assert_eq!(status.mempool_size, 0);
    assert_eq!(status.dag_tips, 0);
    assert!(!status.syncing);
    assert_eq!(status.chain_id, 40204);
    assert_eq!(status.uptime_seconds, 0);
}

// =========================================================================
// EVENT BUS — STRESS & OVERFLOW
// =========================================================================

#[tokio::test]
async fn test_event_bus_overflow_recovery() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();

    // Overflow the 256-event buffer
    for i in 0..300 {
        bus.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: i,
            peer_count: 0,
            syncing: false,
        });
    }

    // Subscriber should recover with a lagged error, then get recent events
    let mut received = 0;
    let mut lagged = 0;
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(_)) => received += 1,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                lagged += 1;
                assert!(n > 0, "Lagged count must be positive");
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            Err(_timeout) => break, // No more events — done
        }
    }
    // Should have gotten a lag notification plus remaining events
    assert!(lagged > 0, "Should have lagged from buffer overflow");
    assert!(received > 0, "Should have received events after lag recovery");
    assert!(received < 300, "Should have missed some events due to overflow");
}

#[tokio::test]
async fn test_subscriber_dropped_during_publish() {
    let bus = EventBus::new();
    {
        let _rx = bus.subscribe(); // subscriber created and immediately dropped
    }
    // Should not panic
    bus.publish(AppEvent::BackgroundError {
        service: "test".into(),
        message: "orphaned".into(),
    });
}

// =========================================================================
// ERROR TYPE — EXHAUSTIVE MATCHING
// =========================================================================

#[test]
fn test_error_matches_exhaustively() {
    // This test ensures all error variants are matchable
    let errors: Vec<AppError> = vec![
        AppError::Node("".into()),
        AppError::Wallet("".into()),
        AppError::Storage("".into()),
        AppError::Network("".into()),
        AppError::Config("".into()),
        AppError::ChainQuery("".into()),
        AppError::ContractCall { contract: "".into(), method: "".into(), reason: "".into() },
        AppError::InsufficientFunds { have: "".into(), need: "".into() },
        AppError::InvalidAddress("".into()),
        AppError::SessionExpired,
        AppError::RateLimited("".into()),
        AppError::ModelNotLoaded("".into()),
        AppError::Editor("".into()),
        AppError::Terminal("".into()),
        AppError::Git("".into()),
        AppError::Compiler("".into()),
        AppError::FileSystem("".into()),
        AppError::BufferNotFound("".into()),
        AppError::SessionNotFound("".into()),
        AppError::Internal(anyhow::anyhow!("internal")),
    ];

    for err in errors {
        match &err {
            AppError::Node(s) => { let _ = s; }
            AppError::Wallet(s) => { let _ = s; }
            AppError::Storage(s) => { let _ = s; }
            AppError::Network(s) => { let _ = s; }
            AppError::Config(s) => { let _ = s; }
            AppError::ChainQuery(s) => { let _ = s; }
            AppError::ContractCall { contract, method, reason } => {
                let _ = (contract, method, reason);
            }
            AppError::InsufficientFunds { have, need } => {
                let _ = (have, need);
            }
            AppError::InvalidAddress(s) => { let _ = s; }
            AppError::SessionExpired => {}
            AppError::RateLimited(s) => { let _ = s; }
            AppError::ModelNotLoaded(s) => { let _ = s; }
            AppError::Editor(s) => { let _ = s; }
            AppError::Terminal(s) => { let _ = s; }
            AppError::Git(s) => { let _ = s; }
            AppError::Compiler(s) => { let _ = s; }
            AppError::FileSystem(s) => { let _ = s; }
            AppError::BufferNotFound(s) => { let _ = s; }
            AppError::SessionNotFound(s) => { let _ = s; }
            AppError::Internal(e) => { let _ = e; }
        }
        // Every variant must produce a non-empty display string
        assert!(!err.to_string().is_empty());
    }
}
