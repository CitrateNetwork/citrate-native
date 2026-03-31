//! End-to-end wallet journey tests using AppCore.
//!
//! These tests exercise the full wallet lifecycle through the headless service
//! layer: create wallet, unlock, send transactions, import mnemonic, lock,
//! and verify events are published to the event bus.
//!
//! No Slint UI is instantiated — we test the service orchestration that the
//! UI shell would call.

use std::sync::Arc;
use tokio::sync::RwLock;

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::services::node_service::{NodeBackend, NodeService};
use citrate_desktop_app::services::wallet_service::{
    Account, CreateAccountResult, WalletBackend, WalletService,
};
use citrate_desktop_app::AppConfig;

// ============================================================================
// Test wallet backend — deterministic, in-memory, no disk or network I/O
// ============================================================================

struct E2eWalletBackend {
    /// Track whether locked or unlocked for realistic behavior
    unlocked: tokio::sync::RwLock<bool>,
    /// Track created accounts for load_accounts
    accounts: tokio::sync::RwLock<Vec<Account>>,
    /// Counter for deterministic address generation
    counter: std::sync::atomic::AtomicU32,
}

impl E2eWalletBackend {
    fn new() -> Self {
        Self {
            unlocked: tokio::sync::RwLock::new(false),
            accounts: tokio::sync::RwLock::new(Vec::new()),
            counter: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

#[async_trait::async_trait]
impl WalletBackend for E2eWalletBackend {
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError> {
        Ok(self.accounts.read().await.clone())
    }

    async fn create_wallet(
        &self,
        _password: &str,
        label: &str,
    ) -> Result<CreateAccountResult, AppError> {
        let idx = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let address = format!("0x{:040x}", idx + 1);
        let account = Account {
            address: address.clone(),
            label: label.to_string(),
            balance: "0".to_string(),
            nonce: 0,
            is_default: idx == 0,
        };
        self.accounts.write().await.push(account);

        Ok(CreateAccountResult {
            address,
            mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
            public_key: format!("{:064x}", idx + 1),
        })
    }

    async fn recover_from_mnemonic(
        &self,
        _mnemonic: &str,
        _password: &str,
        label: &str,
    ) -> Result<CreateAccountResult, AppError> {
        let idx = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let address = format!("0x{:040x}", idx + 100);
        let account = Account {
            address: address.clone(),
            label: label.to_string(),
            balance: "0".to_string(),
            nonce: 0,
            is_default: false,
        };
        self.accounts.write().await.push(account);

        Ok(CreateAccountResult {
            address,
            mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
            public_key: format!("{:064x}", idx + 100),
        })
    }

    async fn unlock(&self, _address: &str, password: &str) -> Result<bool, AppError> {
        if password == "correct_password_123" || password.len() >= 8 {
            *self.unlocked.write().await = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn lock(&self) -> Result<(), AppError> {
        *self.unlocked.write().await = false;
        Ok(())
    }

    async fn send_transaction(
        &self,
        _from: &str,
        _to: &str,
        _value_wei: &str,
        _password: &str,
    ) -> Result<String, AppError> {
        let unlocked = *self.unlocked.read().await;
        if !unlocked {
            return Err(AppError::Wallet(
                "Wallet is locked — unlock first".to_string(),
            ));
        }
        Ok("0xfeedface00000000000000000000000000000000000000000000000000000001".to_string())
    }
}

// ============================================================================
// Test node backend (minimal — wallet tests don't need a real node)
// ============================================================================

struct E2eNodeBackend;

#[async_trait::async_trait]
impl NodeBackend for E2eNodeBackend {
    async fn start_node(&self, _chain_id: u64, _data_dir: &str) -> Result<(), AppError> {
        Ok(())
    }
    async fn stop_node(&self) -> Result<(), AppError> {
        Ok(())
    }
    async fn get_block_height(&self) -> u64 {
        0
    }
    async fn get_peer_count(&self) -> u32 {
        0
    }
    async fn get_mempool_size(&self) -> usize {
        0
    }
    async fn get_balance(&self, _address: &[u8; 20]) -> String {
        "0".to_string()
    }
}

// ============================================================================
// Helper: build an AppCore-like test harness with injected backends
// ============================================================================

struct TestHarness {
    wallet: Arc<WalletService>,
    #[allow(dead_code)]
    node: Arc<NodeService>,
    events: Arc<EventBus>,
}

impl TestHarness {
    fn new() -> Self {
        let events = Arc::new(EventBus::new());
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let wallet = Arc::new(WalletService::with_backend(
            events.clone(),
            Arc::new(E2eWalletBackend::new()),
        ));
        let node = Arc::new(NodeService::with_backend(
            config,
            events.clone(),
            Arc::new(E2eNodeBackend),
        ));
        Self {
            wallet,
            node,
            events,
        }
    }
}

// ============================================================================
// Tests: Create wallet and verify account list
// ============================================================================

#[tokio::test]
async fn test_create_wallet_then_account_appears_in_list() {
    let h = TestHarness::new();

    // Initially no accounts
    assert!(
        h.wallet.is_first_run().await,
        "Should be first run before wallet creation"
    );
    assert!(
        h.wallet.list_accounts().await.is_empty(),
        "Account list should be empty before creation"
    );

    // Create wallet
    let result = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");
    assert!(
        !result.address.is_empty(),
        "Created wallet should have an address"
    );
    assert!(
        result.address.starts_with("0x"),
        "Address should start with 0x"
    );

    // Verify account appears
    let accounts = h.wallet.list_accounts().await;
    assert_eq!(accounts.len(), 1, "Should have exactly one account");
    assert_eq!(
        accounts[0].address, result.address,
        "Account address should match creation result"
    );
    assert_eq!(
        accounts[0].label, "Primary Account",
        "Default label should be 'Primary Account'"
    );
    assert!(accounts[0].is_default, "First account should be default");
    assert!(
        !h.wallet.is_first_run().await,
        "Should no longer be first run after creation"
    );
}

// ============================================================================
// Tests: Create wallet, unlock, verify session
// ============================================================================

#[tokio::test]
async fn test_create_wallet_then_unlock_activates_session() {
    let h = TestHarness::new();

    let result = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    // Verify session is initially inactive
    let status = h.wallet.get_session_status().await;
    assert!(
        !status.is_active,
        "Session should be inactive before unlock"
    );

    // Unlock
    let session = h
        .wallet
        .unlock(&result.address, "strongpassword123")
        .await
        .expect("unlock should succeed");
    assert!(session.is_active, "Session should be active after unlock");
    assert_eq!(
        session.remaining_seconds,
        Some(3600),
        "Session timeout should be 3600s"
    );

    // Verify via get_session_status
    let status = h.wallet.get_session_status().await;
    assert!(
        status.is_active,
        "get_session_status should report active session"
    );
}

// ============================================================================
// Tests: Full send transaction flow
// ============================================================================

#[tokio::test]
async fn test_create_wallet_unlock_send_tx_publishes_event() {
    let h = TestHarness::new();
    let mut rx = h.events.subscribe();

    // Create and unlock
    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");
    h.wallet
        .unlock(&created.address, "strongpassword123")
        .await
        .expect("unlock should succeed");

    // Send transaction
    let to_addr = "0x0000000000000000000000000000000000000002";
    let tx_hash = h
        .wallet
        .send_transaction(&created.address, to_addr, "1000000000000000000", "password")
        .await
        .expect("send should succeed when unlocked");

    assert!(
        !tx_hash.is_empty(),
        "Transaction hash should not be empty"
    );
    assert!(
        tx_hash.starts_with("0x"),
        "Transaction hash should start with 0x"
    );

    // Verify TransactionConfirmed event was published
    let event = rx
        .recv()
        .await
        .expect("should receive TransactionConfirmed event");
    match event {
        AppEvent::TransactionConfirmed {
            tx_hash: hash,
            success,
            ..
        } => {
            assert!(success, "Transaction should be marked successful");
            assert_eq!(
                hash, tx_hash,
                "Event tx_hash should match returned hash"
            );
        }
        other => panic!(
            "Expected TransactionConfirmed event, got {:?}",
            other
        ),
    }
}

// ============================================================================
// Tests: Import mnemonic
// ============================================================================

#[tokio::test]
async fn test_import_mnemonic_account_appears() {
    let h = TestHarness::new();

    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let result = h
        .wallet
        .import_from_mnemonic(mnemonic, "strongpassword123")
        .await
        .expect("mnemonic import should succeed");

    assert!(
        !result.address.is_empty(),
        "Imported account should have an address"
    );

    let accounts = h.wallet.list_accounts().await;
    assert_eq!(
        accounts.len(),
        1,
        "Should have one account after import"
    );
    assert_eq!(
        accounts[0].label, "Imported Account",
        "Imported account should have 'Imported Account' label"
    );
    assert!(
        accounts[0].is_default,
        "First imported account should be default (was empty before)"
    );
}

#[tokio::test]
async fn test_import_mnemonic_too_short_rejected() {
    let h = TestHarness::new();

    let short_mnemonic = "abandon abandon abandon"; // Only 3 words
    let result = h
        .wallet
        .import_from_mnemonic(short_mnemonic, "strongpassword123")
        .await;

    assert!(
        result.is_err(),
        "Mnemonic with fewer than 12 words should be rejected"
    );
}

#[tokio::test]
async fn test_import_mnemonic_empty_rejected() {
    let h = TestHarness::new();

    let result = h
        .wallet
        .import_from_mnemonic("", "strongpassword123")
        .await;

    assert!(
        result.is_err(),
        "Empty mnemonic should be rejected"
    );
}

#[tokio::test]
async fn test_import_mnemonic_short_password_rejected() {
    let h = TestHarness::new();

    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let result = h
        .wallet
        .import_from_mnemonic(mnemonic, "short")
        .await;

    assert!(
        result.is_err(),
        "Mnemonic import with short password should be rejected"
    );
}

// ============================================================================
// Tests: Lock and verify session inactive, operations fail
// ============================================================================

#[tokio::test]
async fn test_lock_then_operations_fail() {
    let h = TestHarness::new();

    // Create, unlock, then lock
    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");
    h.wallet
        .unlock(&created.address, "strongpassword123")
        .await
        .expect("unlock should succeed");
    h.wallet.lock().await.expect("lock should succeed");

    // Verify session is inactive
    let status = h.wallet.get_session_status().await;
    assert!(
        !status.is_active,
        "Session should be inactive after lock"
    );

    // Send should fail with SessionExpired
    let send_result = h
        .wallet
        .send_transaction(
            &created.address,
            "0x0000000000000000000000000000000000000002",
            "1000",
            "password",
        )
        .await;

    match send_result {
        Err(AppError::SessionExpired) => { /* expected */ }
        other => panic!(
            "Expected SessionExpired error after lock, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn test_send_without_unlock_fails() {
    let h = TestHarness::new();

    // Create but never unlock
    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    let result = h
        .wallet
        .send_transaction(
            &created.address,
            "0x0000000000000000000000000000000000000002",
            "1000",
            "password",
        )
        .await;

    match result {
        Err(AppError::SessionExpired) => { /* expected */ }
        other => panic!(
            "Expected SessionExpired when sending without unlock, got {:?}",
            other
        ),
    }
}

// ============================================================================
// Tests: Multiple accounts and selection
// ============================================================================

#[tokio::test]
async fn test_multiple_accounts_created_sequentially() {
    let h = TestHarness::new();

    // Create first account
    let first = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("first wallet creation should succeed");

    // Import a second via mnemonic
    let mnemonic = "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong";
    let second = h
        .wallet
        .import_from_mnemonic(mnemonic, "strongpassword123")
        .await
        .expect("mnemonic import should succeed");

    let accounts = h.wallet.list_accounts().await;
    assert_eq!(accounts.len(), 2, "Should have two accounts");

    // First account should have its own address
    assert_eq!(accounts[0].address, first.address);
    assert!(accounts[0].is_default, "First created account should be default");

    // Second account should have a different address
    assert_eq!(accounts[1].address, second.address);
    assert_ne!(
        first.address, second.address,
        "Two accounts should have different addresses"
    );
}

#[tokio::test]
async fn test_primary_address_returns_first_account() {
    let h = TestHarness::new();

    // No primary address before any account
    assert!(
        h.wallet.get_primary_address().await.is_none(),
        "No primary address before wallet creation"
    );

    // Create wallet
    let result = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    let primary = h
        .wallet
        .get_primary_address()
        .await
        .expect("primary address should exist after creation");
    assert_eq!(
        primary, result.address,
        "Primary address should match created account"
    );
}

// ============================================================================
// Tests: Password validation
// ============================================================================

#[tokio::test]
async fn test_create_wallet_short_password_rejected() {
    let h = TestHarness::new();

    let result = h.wallet.create_wallet("short").await;
    assert!(
        result.is_err(),
        "Password shorter than 8 chars should be rejected"
    );

    // Wallet should still be in first-run state
    assert!(h.wallet.is_first_run().await);
}

#[tokio::test]
async fn test_create_wallet_empty_password_rejected() {
    let h = TestHarness::new();

    let result = h.wallet.create_wallet("").await;
    assert!(result.is_err(), "Empty password should be rejected");
}

#[tokio::test]
async fn test_create_wallet_exactly_8_chars_accepted() {
    let h = TestHarness::new();

    let result = h.wallet.create_wallet("12345678").await;
    assert!(
        result.is_ok(),
        "Password with exactly 8 chars should be accepted"
    );
}

#[tokio::test]
async fn test_unlock_empty_password_rejected() {
    let h = TestHarness::new();

    let result = h.wallet.unlock("0xabc", "").await;
    assert!(result.is_err(), "Empty password should be rejected for unlock");
}

#[tokio::test]
async fn test_unlock_wrong_password_rejected() {
    let h = TestHarness::new();

    // The E2eWalletBackend rejects passwords shorter than 8 chars
    let result = h.wallet.unlock("0xabc", "wrong").await;
    assert!(
        result.is_err(),
        "Wrong/short password should fail unlock"
    );
}

// ============================================================================
// Tests: Lock/unlock cycles
// ============================================================================

#[tokio::test]
async fn test_multiple_lock_unlock_cycles() {
    let h = TestHarness::new();

    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    for i in 0..5 {
        h.wallet
            .unlock(&created.address, "strongpassword123")
            .await
            .unwrap_or_else(|_| panic!("unlock cycle {} should succeed", i));

        let status = h.wallet.get_session_status().await;
        assert!(status.is_active, "Session should be active in cycle {}", i);

        h.wallet
            .lock()
            .await
            .unwrap_or_else(|_| panic!("lock cycle {} should succeed", i));

        let status = h.wallet.get_session_status().await;
        assert!(!status.is_active, "Session should be inactive after lock in cycle {}", i);
    }
}

// ============================================================================
// Tests: Balance formatting through the display pipeline
// ============================================================================

#[tokio::test]
async fn test_wei_to_salt_conversion_in_display_pipeline() {
    use citrate_wallet_core::format::{wei_to_salt, format_salt_display};

    // Simulate the pipeline: backend returns wei string, UI converts to SALT display
    let wei_values: Vec<(&str, &str, &str)> = vec![
        ("0", "0", "0 SALT"),
        ("1000000000000000000", "1", "1 SALT"),
        ("500000000000000000", "0.5", "0.5 SALT"),
        ("2500000000000000000", "2.5", "2.5 SALT"),
    ];

    for (wei_str, expected_salt, expected_display) in &wei_values {
        let wei: u128 = wei_str
            .parse()
            .unwrap_or_else(|_| panic!("parsing wei string '{}' should succeed", wei_str));
        let salt = wei_to_salt(wei);
        assert_eq!(
            &salt, expected_salt,
            "wei_to_salt({}) should produce {}",
            wei_str, expected_salt
        );

        let display = format_salt_display(wei);
        assert_eq!(
            &display, expected_display,
            "format_salt_display({}) should produce {}",
            wei_str, expected_display
        );
    }
}

// ============================================================================
// Tests: Mnemonic returned on create has expected word count
// ============================================================================

#[tokio::test]
async fn test_created_wallet_returns_12_word_mnemonic() {
    let h = TestHarness::new();

    let result = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    let word_count = result.mnemonic.split_whitespace().count();
    assert!(
        word_count >= 12,
        "Mnemonic should have at least 12 words, got {}",
        word_count
    );
}

// ============================================================================
// Tests: Event bus receives transaction events
// ============================================================================

#[tokio::test]
async fn test_send_multiple_transactions_publishes_multiple_events() {
    let h = TestHarness::new();
    let mut rx = h.events.subscribe();

    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");
    h.wallet
        .unlock(&created.address, "strongpassword123")
        .await
        .expect("unlock should succeed");

    let to_addr = "0x0000000000000000000000000000000000000099";

    // Send 3 transactions
    for i in 0..3 {
        let amount = format!("{}", (i + 1) * 1_000_000_000_000_000_000u64);
        h.wallet
            .send_transaction(&created.address, to_addr, &amount, "password")
            .await
            .unwrap_or_else(|_| panic!("send {} should succeed", i));
    }

    // Verify 3 TransactionConfirmed events
    for i in 0..3 {
        let event = rx
            .recv()
            .await
            .unwrap_or_else(|_| panic!("should receive event {}", i));
        match event {
            AppEvent::TransactionConfirmed { success, .. } => {
                assert!(success, "Transaction {} should be successful", i);
            }
            other => panic!("Expected TransactionConfirmed for tx {}, got {:?}", i, other),
        }
    }
}

// ============================================================================
// Tests: Load from disk (empty backend)
// ============================================================================

#[tokio::test]
async fn test_load_from_disk_empty_backend_stays_first_run() {
    let events = Arc::new(EventBus::new());
    // Fresh backend with no accounts
    let wallet = WalletService::with_backend(events, Arc::new(E2eWalletBackend::new()));

    wallet
        .load_from_disk()
        .await
        .expect("load_from_disk should succeed");

    assert!(
        wallet.is_first_run().await,
        "Empty backend should still be first run after load"
    );
}

// ============================================================================
// Tests: Full lifecycle — create, unlock, send, lock, re-unlock, send again
// ============================================================================

#[tokio::test]
async fn test_full_wallet_lifecycle() {
    let h = TestHarness::new();

    // 1. Create
    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");
    assert!(!h.wallet.is_first_run().await);

    // 2. Unlock
    let session = h
        .wallet
        .unlock(&created.address, "strongpassword123")
        .await
        .expect("unlock should succeed");
    assert!(session.is_active);

    // 3. Send
    let tx1 = h
        .wallet
        .send_transaction(
            &created.address,
            "0x0000000000000000000000000000000000000002",
            "1000000000000000000",
            "password",
        )
        .await
        .expect("first send should succeed");
    assert!(!tx1.is_empty());

    // 4. Lock
    h.wallet.lock().await.expect("lock should succeed");
    assert!(!h.wallet.get_session_status().await.is_active);

    // 5. Send while locked fails
    let locked_send = h
        .wallet
        .send_transaction(
            &created.address,
            "0x0000000000000000000000000000000000000002",
            "500000000000000000",
            "password",
        )
        .await;
    assert!(
        matches!(locked_send, Err(AppError::SessionExpired)),
        "Send while locked should return SessionExpired"
    );

    // 6. Re-unlock
    h.wallet
        .unlock(&created.address, "strongpassword123")
        .await
        .expect("re-unlock should succeed");

    // 7. Send again
    let tx2 = h
        .wallet
        .send_transaction(
            &created.address,
            "0x0000000000000000000000000000000000000003",
            "250000000000000000",
            "password",
        )
        .await
        .expect("send after re-unlock should succeed");
    assert!(!tx2.is_empty());
}

// ============================================================================
// Tests: Import then create gives two accounts
// ============================================================================

#[tokio::test]
async fn test_import_then_create_produces_two_accounts() {
    let h = TestHarness::new();

    // Import first
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let imported = h
        .wallet
        .import_from_mnemonic(mnemonic, "strongpassword123")
        .await
        .expect("import should succeed");

    // Create second
    let created = h
        .wallet
        .create_wallet("strongpassword123")
        .await
        .expect("create should succeed");

    let accounts = h.wallet.list_accounts().await;
    assert_eq!(accounts.len(), 2, "Should have two accounts");

    // Imported account should be first
    assert_eq!(accounts[0].address, imported.address);
    assert_eq!(accounts[0].label, "Imported Account");

    // Created account should be second
    assert_eq!(accounts[1].address, created.address);
    assert_eq!(accounts[1].label, "Primary Account");

    // Addresses must differ
    assert_ne!(imported.address, created.address);
}

// ============================================================================
// Tests: Session status fields
// ============================================================================

#[tokio::test]
async fn test_session_status_fields_after_unlock() {
    let h = TestHarness::new();

    h.wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    // Before unlock
    let before = h.wallet.get_session_status().await;
    assert!(!before.is_active);
    assert!(before.remaining_seconds.is_none());
    assert!(!before.is_locked_out);

    // After unlock
    h.wallet
        .unlock("0x0000000000000000000000000000000000000001", "strongpassword123")
        .await
        .expect("unlock should succeed");
    let after = h.wallet.get_session_status().await;
    assert!(after.is_active);
    assert_eq!(after.remaining_seconds, Some(3600));
    assert!(!after.is_locked_out);
}

// ============================================================================
// Tests: Account struct field accessibility
// ============================================================================

#[tokio::test]
async fn test_account_fields_accessible_for_ui_binding() {
    let h = TestHarness::new();

    h.wallet
        .create_wallet("strongpassword123")
        .await
        .expect("wallet creation should succeed");

    let accounts = h.wallet.list_accounts().await;
    let account = &accounts[0];

    // Every field that the Slint UI binds to must be accessible
    let _addr: &str = &account.address;
    let _label: &str = &account.label;
    let _balance: &str = &account.balance;
    let _nonce: u64 = account.nonce;
    let _is_default: bool = account.is_default;

    // Clone must work (used in push_accounts_to_ui)
    let _cloned = account.clone();
    let _debug = format!("{:?}", account);
}
