//! Wallet integration tests — end-to-end wallet flows.
//!
//! Tests the full wallet lifecycle: create, list, switch, send, lock/unlock.
//! Each test uses a fresh WalletService with the test backend.

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::services::wallet_service::{
    Account, CreateAccountResult, WalletBackend, WalletService,
};
use std::sync::Arc;

struct IntegrationWalletBackend {
    accounts: tokio::sync::RwLock<Vec<Account>>,
    counter: std::sync::atomic::AtomicU32,
}

impl IntegrationWalletBackend {
    fn new() -> Self {
        Self {
            accounts: tokio::sync::RwLock::new(Vec::new()),
            counter: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

#[async_trait::async_trait]
impl WalletBackend for IntegrationWalletBackend {
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError> {
        Ok(self.accounts.read().await.clone())
    }

    async fn create_wallet(&self, password: &str, _label: &str) -> Result<CreateAccountResult, AppError> {
        if password.len() < 8 {
            return Err(AppError::Wallet("Password too short".into()));
        }
        let idx = self.counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let address = format!("0x{:040x}", idx + 1);
        self.accounts.write().await.push(Account {
            address: address.clone(),
            label: format!("Account {}", idx + 1),
            balance: "0".to_string(),
            nonce: 0,
            is_default: idx == 0,
        });
        Ok(CreateAccountResult {
            address,
            mnemonic: "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
            public_key: format!("0xpub{:040x}", idx + 1),
        })
    }

    async fn unlock(&self, _address: &str, password: &str) -> Result<bool, AppError> {
        if password.is_empty() {
            return Err(AppError::Wallet("Empty password".into()));
        }
        Ok(true)
    }

    async fn lock(&self) -> Result<(), AppError> { Ok(()) }

    async fn send_transaction(&self, from: &str, to: &str, value_wei: &str, _password: &str) -> Result<String, AppError> {
        if from.is_empty() { return Err(AppError::Wallet("Empty from".into())); }
        if to.is_empty() { return Err(AppError::Wallet("Empty to".into())); }
        if value_wei.is_empty() { return Err(AppError::Wallet("Empty value".into())); }
        let h: u64 = format!("{}{}{}", from, to, value_wei).bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64));
        Ok(format!("0x{:064x}", h))
    }
}

fn test_wallet() -> WalletService {
    WalletService::with_backend(Arc::new(EventBus::new()), Arc::new(IntegrationWalletBackend::new()))
}

// === LIFECYCLE ===

#[tokio::test]
async fn test_first_run_is_empty() {
    let svc = test_wallet();
    assert!(svc.is_first_run().await);
    assert_eq!(svc.list_accounts().await.len(), 0);
}

#[tokio::test]
async fn test_create_wallet_populates_list() {
    let svc = test_wallet();
    let r = svc.create_wallet("strongpassword123").await.expect("create");
    assert!(!r.address.is_empty());
    assert!(!r.mnemonic.is_empty());
    assert_eq!(svc.list_accounts().await.len(), 1);
}

#[tokio::test]
async fn test_create_multiple_accounts() {
    let svc = test_wallet();
    let r1 = svc.create_wallet("strongpassword111").await.expect("1");
    let r2 = svc.create_wallet("strongpassword222").await.expect("2");
    let r3 = svc.create_wallet("strongpassword333").await.expect("3");
    assert_eq!(svc.list_accounts().await.len(), 3);
    assert_ne!(r1.address, r2.address);
    assert_ne!(r2.address, r3.address);
}

#[tokio::test]
async fn test_first_account_is_default() {
    let svc = test_wallet();
    svc.create_wallet("strongpassword123").await.expect("create");
    assert!(svc.list_accounts().await[0].is_default);
}

#[tokio::test]
async fn test_not_first_run_after_create() {
    let svc = test_wallet();
    svc.create_wallet("strongpassword123").await.expect("create");
    assert!(!svc.is_first_run().await);
}

// === SEND ===

#[tokio::test]
async fn test_send_requires_session() {
    // P960-G fix: create_wallet auto-activates the session, so we
    // must lock() to reach the "send without session" condition.
    // The semantic property — "sending while locked fails" — is
    // unchanged.
    let svc = test_wallet();
    svc.create_wallet("strongpassword123").await.expect("create");
    svc.lock().await.expect("lock");
    let r = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await;
    assert!(r.is_err(), "Send while locked should fail");
}

#[tokio::test]
async fn test_send_with_session_succeeds() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    let r = svc.send_transaction(&c.address, "0xrecipient", "1000", "pwd").await;
    assert!(r.is_ok());
    assert!(r.expect("hash").starts_with("0x"));
}

#[tokio::test]
async fn test_send_empty_to_fails() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    assert!(svc.send_transaction(&c.address, "", "1000", "pwd").await.is_err());
}

#[tokio::test]
async fn test_send_empty_value_fails() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    assert!(svc.send_transaction(&c.address, "0xto", "", "pwd").await.is_err());
}

// === LOCK / UNLOCK ===

#[tokio::test]
async fn test_unlock_activates_session() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    assert!(svc.get_session_status().await.is_active);
}

#[tokio::test]
async fn test_lock_deactivates_session() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    svc.lock().await.expect("lock");
    assert!(!svc.get_session_status().await.is_active);
}

#[tokio::test]
async fn test_send_after_lock_fails() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    svc.lock().await.expect("lock");
    assert!(svc.send_transaction(&c.address, "0xto", "1000", "pwd").await.is_err());
}

#[tokio::test]
async fn test_unlock_empty_password_fails() {
    let svc = test_wallet();
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    assert!(svc.unlock(&c.address, "").await.is_err());
}

// === PASSWORD VALIDATION ===

#[tokio::test]
async fn test_short_password_rejected() {
    let svc = test_wallet();
    assert!(svc.create_wallet("short").await.is_err());
}

#[tokio::test]
async fn test_empty_password_rejected() {
    let svc = test_wallet();
    assert!(svc.create_wallet("").await.is_err());
}

// === ACCOUNT SELECTION ===

#[tokio::test]
async fn test_primary_address_is_first() {
    let svc = test_wallet();
    let r1 = svc.create_wallet("strongpassword123").await.expect("1");
    svc.create_wallet("strongpassword456").await.expect("2");
    assert_eq!(svc.get_primary_address().await, Some(r1.address));
}

#[tokio::test]
async fn test_accounts_preserve_order() {
    let svc = test_wallet();
    let r1 = svc.create_wallet("strongpassword111").await.expect("1");
    let r2 = svc.create_wallet("strongpassword222").await.expect("2");
    let r3 = svc.create_wallet("strongpassword333").await.expect("3");
    let accts = svc.list_accounts().await;
    assert_eq!(accts[0].address, r1.address);
    assert_eq!(accts[1].address, r2.address);
    assert_eq!(accts[2].address, r3.address);
}

// === EVENTS ===

#[tokio::test]
async fn test_send_publishes_event() {
    let events = Arc::new(EventBus::new());
    let mut rx = events.subscribe();
    let svc = WalletService::with_backend(events, Arc::new(IntegrationWalletBackend::new()));
    let c = svc.create_wallet("strongpassword123").await.expect("create");
    svc.unlock(&c.address, "strongpassword123").await.expect("unlock");
    svc.send_transaction(&c.address, "0xto", "1000", "pwd").await.expect("send");
    let event = rx.recv().await.expect("event");
    assert!(matches!(event, AppEvent::TransactionConfirmed { .. }));
}

// === MNEMONIC ===

#[tokio::test]
async fn test_mnemonic_is_12_words() {
    let svc = test_wallet();
    let r = svc.create_wallet("strongpassword123").await.expect("create");
    assert!(r.mnemonic.split_whitespace().count() >= 12);
}

// === CONCURRENCY ===

#[tokio::test]
async fn test_concurrent_creation() {
    let svc = Arc::new(test_wallet());
    let mut handles = vec![];
    for i in 0..5 {
        let s = svc.clone();
        handles.push(tokio::spawn(async move {
            s.create_wallet(&format!("strongpassword{:03}", i)).await
        }));
    }
    for h in handles { h.await.expect("join").expect("create"); }
    assert_eq!(svc.list_accounts().await.len(), 5);
}

#[tokio::test]
async fn test_concurrent_reads() {
    let svc = Arc::new(test_wallet());
    svc.create_wallet("strongpassword123").await.expect("create");
    let mut handles = vec![];
    for _ in 0..10 {
        let s = svc.clone();
        handles.push(tokio::spawn(async move { assert!(!s.list_accounts().await.is_empty()); }));
    }
    for h in handles { h.await.expect("join"); }
}
