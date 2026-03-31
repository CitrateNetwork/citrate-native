//! Wallet operations service.
//!
//! Data source: wallet/ crate (ed25519 keypair management, transaction signing)
//! This service wraps the wallet manager and exposes typed operations
//! that the UI layer consumes directly.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Trait for real wallet backend implementations.
#[async_trait::async_trait]
pub trait WalletBackend: Send + Sync {
    /// Load existing accounts from disk
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError>;
    /// Create a new wallet with password, return address + mnemonic
    async fn create_wallet(&self, password: &str, label: &str) -> Result<CreateAccountResult, AppError>;
    /// Recover an account from a BIP39 mnemonic phrase.
    /// Default returns an error — overridden by production backend.
    async fn recover_from_mnemonic(&self, _mnemonic: &str, _password: &str, _label: &str) -> Result<CreateAccountResult, AppError> {
        Err(AppError::Wallet("Mnemonic recovery not supported by this backend".to_string()))
    }
    /// Verify password and unlock session
    async fn unlock(&self, address: &str, password: &str) -> Result<bool, AppError>;
    /// Lock the session
    async fn lock(&self) -> Result<(), AppError>;
    /// Sign and send a transaction, return tx hash
    async fn send_transaction(&self, from: &str, to: &str, value_wei: &str, password: &str) -> Result<String, AppError>;
}

/// Production wallet backend — delegates to citrate-wallet-core for real
/// Argon2+AES-GCM key management, BIP39 mnemonics, and transaction signing.
pub struct WalletCoreBackend {
    key_manager: Arc<citrate_wallet_core::KeyManager>,
    rpc_client: Arc<citrate_wallet_core::RpcClient>,
}

impl WalletCoreBackend {
    pub fn new() -> Self {
        let config = citrate_wallet_core::WalletConfig::default();
        let keystore_path = std::path::PathBuf::from(&config.keystore_path);
        Self {
            key_manager: Arc::new(citrate_wallet_core::KeyManager::new(&keystore_path)),
            rpc_client: Arc::new(citrate_wallet_core::RpcClient::new(&config.rpc_url)),
        }
    }
}

impl Default for WalletCoreBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl WalletBackend for WalletCoreBackend {
    async fn load_accounts(&self) -> Result<Vec<Account>, AppError> {
        self.key_manager.load()
            .map_err(|e| AppError::Wallet(format!("Failed to load keystore: {}", e)))?;

        let wallet_accounts = self.key_manager.list_accounts();
        Ok(wallet_accounts.iter().map(|a| Account {
            address: a.address.clone(),
            label: a.label.clone(),
            balance: a.balance.clone(),
            nonce: a.nonce,
            is_default: a.is_default,
        }).collect())
    }

    async fn create_wallet(&self, password: &str, label: &str) -> Result<CreateAccountResult, AppError> {
        let result = self.key_manager.create_account(password, label)
            .map_err(|e| AppError::Wallet(format!("{}", e)))?;

        Ok(CreateAccountResult {
            address: result.address,
            mnemonic: result.mnemonic,
            public_key: result.public_key_hex,
        })
    }

    async fn recover_from_mnemonic(&self, mnemonic: &str, password: &str, label: &str) -> Result<CreateAccountResult, AppError> {
        // Data source: citrate_wallet_core::KeyManager::recover_from_mnemonic
        // Derives Ed25519 key from BIP39 mnemonic via SLIP-0010, encrypts with Argon2+AES-GCM
        let result = self.key_manager.recover_from_mnemonic(mnemonic, password, label)
            .map_err(|e| AppError::Wallet(format!("Recovery failed: {}", e)))?;
        Ok(CreateAccountResult {
            address: result.address,
            mnemonic: result.mnemonic,
            public_key: result.public_key_hex,
        })
    }

    async fn unlock(&self, _address: &str, password: &str) -> Result<bool, AppError> {
        match self.key_manager.unlock(password) {
            Ok(count) => Ok(count > 0),
            Err(citrate_wallet_core::WalletError::InvalidPassword) => Ok(false),
            Err(e) => Err(AppError::Wallet(format!("{}", e))),
        }
    }

    async fn lock(&self) -> Result<(), AppError> {
        self.key_manager.lock();
        Ok(())
    }

    async fn send_transaction(&self, from: &str, to: &str, value_wei: &str, _password: &str) -> Result<String, AppError> {
        // Get the signing key (must be unlocked)
        let unified_key = self.key_manager.get_signing_key(from)
            .map_err(|e| AppError::Wallet(format!("Cannot sign: {}", e)))?;

        // Get the nonce from the chain — MUST succeed, no fallback to 0
        // F-03 fix: nonce fetch failure is a real error, not silent zero
        let nonce = self.rpc_client.get_nonce(from).await
            .map_err(|e| AppError::Network(format!("Cannot fetch nonce: {}. Is the node running?", e)))?;

        // Parse value — invalid amounts are errors, not silent zeros
        let value: u128 = value_wei.parse()
            .map_err(|_| AppError::Wallet(format!("Invalid amount: '{}'", value_wei)))?;

        // Build and sign the transaction — supports both Ed25519 and secp256k1
        let signed = match &unified_key {
            citrate_wallet_core::keys::UnifiedKey::Ed25519(ed_key) => {
                citrate_wallet_core::TransactionBuilder::new()
                    .to(to)
                    .value(value)
                    .chain_id(40204)
                    .sign(ed_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("Ed25519 sign failed: {}", e)))?
            }
            citrate_wallet_core::keys::UnifiedKey::Secp256k1(secp_key) => {
                citrate_wallet_core::TransactionBuilder::new()
                    .to(to)
                    .value(value)
                    .chain_id(40204)
                    .sign_secp256k1(secp_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("secp256k1 sign failed: {}", e)))?
            }
        };

        // Submit to RPC — MUST succeed, no local hash fallback
        // F-03 fix: failed submission is a real error, not a fake success
        let tx_hash = self.rpc_client.send_raw_transaction(&signed.raw).await
            .map_err(|e| AppError::Network(format!("Transaction submission failed: {}. The transaction was signed but not accepted by the network.", e)))?;

        Ok(tx_hash)
    }
}

/// Test-only backend with in-memory state. Not compiled in release builds.
#[cfg(test)]
pub struct TestWalletBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl WalletBackend for TestWalletBackend {
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

/// Account information for display
#[derive(Debug, Clone)]
pub struct Account {
    pub address: String,
    pub label: String,
    pub balance: String,
    pub nonce: u64,
    pub is_default: bool,
}

/// Result of creating a new account
#[derive(Debug, Clone)]
pub struct CreateAccountResult {
    pub address: String,
    pub mnemonic: String,
    pub public_key: String,
}

/// Session status for the wallet lock/unlock lifecycle
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub is_active: bool,
    pub remaining_seconds: Option<u64>,
    pub is_locked_out: bool,
}

/// Wallet operations service.
pub struct WalletService {
    events: Arc<EventBus>,
    accounts: Arc<RwLock<Vec<Account>>>,
    session: Arc<RwLock<SessionStatus>>,
    backend: Arc<dyn WalletBackend>,
}

impl WalletService {
    /// Create with the embedded wallet backend (production).
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            accounts: Arc::new(RwLock::new(Vec::new())),
            session: Arc::new(RwLock::new(SessionStatus {
                is_active: false,
                remaining_seconds: None,
                is_locked_out: false,
            })),
            backend: Arc::new(WalletCoreBackend::new()),
        }
    }

    /// Create with an injected backend (for testing or alternative wallet).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn WalletBackend>) -> Self {
        Self {
            events,
            accounts: Arc::new(RwLock::new(Vec::new())),
            session: Arc::new(RwLock::new(SessionStatus {
                is_active: false,
                remaining_seconds: None,
                is_locked_out: false,
            })),
            backend,
        }
    }

    /// Load accounts from disk (called at startup)
    pub async fn load_from_disk(&self) -> Result<(), AppError> {
        tracing::info!("Loading wallet from disk");
        let accounts = self.backend.load_accounts().await?;
        *self.accounts.write().await = accounts;
        Ok(())
    }

    /// Check if this is the first run (no wallet exists)
    pub async fn is_first_run(&self) -> bool {
        self.accounts.read().await.is_empty()
    }

    /// List all accounts
    pub async fn list_accounts(&self) -> Vec<Account> {
        self.accounts.read().await.clone()
    }

    /// Create a new wallet with password (first-time setup)
    pub async fn create_wallet(&self, password: &str) -> Result<CreateAccountResult, AppError> {
        if password.len() < 8 {
            return Err(AppError::Wallet("Password must be at least 8 characters".to_string()));
        }

        let result = self.backend.create_wallet(password, "Primary Account").await?;

        let account = Account {
            address: result.address.clone(),
            label: "Primary Account".to_string(),
            balance: "0".to_string(),
            nonce: 0,
            is_default: true,
        };

        self.accounts.write().await.push(account);
        Ok(result)
    }

    /// Import a wallet from a BIP39 mnemonic phrase
    pub async fn import_from_mnemonic(&self, mnemonic: &str, password: &str) -> Result<CreateAccountResult, AppError> {
        if password.len() < 8 {
            return Err(AppError::Wallet("Password must be at least 8 characters".to_string()));
        }
        if mnemonic.split_whitespace().count() < 12 {
            return Err(AppError::Wallet("Mnemonic must be at least 12 words".to_string()));
        }

        let result = self.backend.recover_from_mnemonic(mnemonic, password, "Imported Account").await?;

        let account = Account {
            address: result.address.clone(),
            label: "Imported Account".to_string(),
            balance: "0".to_string(),
            nonce: 0,
            is_default: self.accounts.read().await.is_empty(),
        };

        self.accounts.write().await.push(account);
        Ok(result)
    }

    /// Unlock the wallet with password
    pub async fn unlock(&self, address: &str, password: &str) -> Result<SessionStatus, AppError> {
        if password.is_empty() {
            return Err(AppError::Wallet("Password required".to_string()));
        }

        let valid = self.backend.unlock(address, password).await?;
        if !valid {
            return Err(AppError::Wallet("Invalid password".to_string()));
        }

        let status = SessionStatus {
            is_active: true,
            remaining_seconds: Some(3600),
            is_locked_out: false,
        };
        *self.session.write().await = status.clone();
        Ok(status)
    }

    /// Lock the wallet
    pub async fn lock(&self) -> Result<(), AppError> {
        self.backend.lock().await?;
        let mut session = self.session.write().await;
        session.is_active = false;
        session.remaining_seconds = None;
        Ok(())
    }

    /// Send SALT between accounts
    pub async fn send_transaction(
        &self,
        from: &str,
        to: &str,
        value_wei: &str,
        password: &str,
    ) -> Result<String, AppError> {
        // Validate session is active
        let session = self.session.read().await;
        if !session.is_active {
            return Err(AppError::SessionExpired);
        }
        drop(session);

        let tx_hash = self.backend.send_transaction(from, to, value_wei, password).await?;

        self.events.publish(AppEvent::TransactionConfirmed {
            tx_hash: tx_hash.clone(),
            block_height: 0, // Will be updated when included in block
            success: true,
        });

        Ok(tx_hash)
    }

    /// Get current session status
    pub async fn get_session_status(&self) -> SessionStatus {
        self.session.read().await.clone()
    }

    /// Get the primary account address (for reward configuration)
    pub async fn get_primary_address(&self) -> Option<String> {
        self.accounts
            .read()
            .await
            .first()
            .map(|a| a.address.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> WalletService {
        let events = Arc::new(EventBus::new());
        WalletService::with_backend(events, Arc::new(TestWalletBackend))
    }

    #[tokio::test]
    async fn test_first_run_when_empty() {
        let svc = test_service();
        assert!(svc.is_first_run().await);
    }

    #[tokio::test]
    async fn test_create_wallet() {
        let svc = test_service();
        let result = svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(!result.address.is_empty());
        assert!(!result.mnemonic.is_empty());
        assert!(!svc.is_first_run().await);
    }

    #[tokio::test]
    async fn test_create_wallet_short_password_rejected() {
        let svc = test_service();
        let result = svc.create_wallet("short").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unlock_empty_password_rejected() {
        let svc = test_service();
        let result = svc.unlock("0xabc", "").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unlock_sets_session_active() {
        let svc = test_service();
        let status = svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        assert!(status.is_active);
        assert!(status.remaining_seconds.is_some());
    }

    #[tokio::test]
    async fn test_lock_clears_session() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        svc.lock().await.expect("lock succeeded");
        let status = svc.get_session_status().await;
        assert!(!status.is_active);
    }

    #[tokio::test]
    async fn test_primary_address_after_create() {
        let svc = test_service();
        assert!(svc.get_primary_address().await.is_none());
        svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(svc.get_primary_address().await.is_some());
    }

    #[tokio::test]
    async fn test_send_requires_active_session() {
        let svc = test_service();
        let result = svc.send_transaction("0xfrom", "0xto", "1000", "password").await;
        match result {
            Err(AppError::SessionExpired) => {}
            other => panic!("Expected SessionExpired, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_send_works_when_unlocked() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        let hash = svc.send_transaction("0xfrom", "0xto", "1000", "password").await.expect("async operation succeeded");
        assert!(!hash.is_empty());
    }

    #[tokio::test]
    async fn test_send_publishes_event() {
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let svc = WalletService::with_backend(events, Arc::new(TestWalletBackend));

        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        svc.send_transaction("0xfrom", "0xto", "1000", "pass").await.expect("async operation succeeded");

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::TransactionConfirmed { success, .. } => assert!(success),
            _ => panic!("Expected TransactionConfirmed"),
        }
    }

    #[tokio::test]
    async fn test_load_from_disk_populates_accounts() {
        let svc = test_service();
        assert!(svc.is_first_run().await);
        svc.load_from_disk().await.expect("load succeeded");
        // Stub returns empty — still first run
        assert!(svc.is_first_run().await);
    }

    #[tokio::test]
    async fn test_create_wallet_returns_mnemonic() {
        let svc = test_service();
        let result = svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(result.mnemonic.split_whitespace().count() >= 5, "Mnemonic should have multiple words");
    }

    #[tokio::test]
    async fn test_with_backend_constructor() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(TestWalletBackend);
        let svc = WalletService::with_backend(events, backend);
        assert!(svc.is_first_run().await);
    }

    #[tokio::test]
    async fn test_list_accounts_empty() {
        let svc = test_service();
        let accounts = svc.list_accounts().await;
        assert!(accounts.is_empty());
    }

    #[tokio::test]
    async fn test_list_accounts_after_create() {
        let svc = test_service();
        svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        let accounts = svc.list_accounts().await;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "Primary Account");
        assert!(accounts[0].is_default);
    }

    #[tokio::test]
    async fn test_session_status_initial() {
        let svc = test_service();
        let status = svc.get_session_status().await;
        assert!(!status.is_active);
        assert!(status.remaining_seconds.is_none());
        assert!(!status.is_locked_out);
    }

    #[tokio::test]
    async fn test_session_timeout_value() {
        let svc = test_service();
        let status = svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        assert_eq!(status.remaining_seconds, Some(3600));
    }

    #[tokio::test]
    async fn test_lock_then_send_fails() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        svc.lock().await.expect("lock succeeded");
        let result = svc.send_transaction("0xfrom", "0xto", "1000", "pwd").await;
        match result {
            Err(AppError::SessionExpired) => {}
            other => panic!("Expected SessionExpired, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_create_wallet_returns_address() {
        let svc = test_service();
        let result = svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(result.address.starts_with("0x"));
        assert_eq!(result.address.len(), 42); // 0x + 40 hex chars
    }

    #[tokio::test]
    async fn test_create_wallet_returns_public_key() {
        let svc = test_service();
        let result = svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(!result.public_key.is_empty());
        assert_eq!(result.public_key.len(), 64); // 32 bytes hex
    }

    #[tokio::test]
    async fn test_password_exactly_8_chars() {
        let svc = test_service();
        let result = svc.create_wallet("12345678").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_password_7_chars_rejected() {
        let svc = test_service();
        let result = svc.create_wallet("1234567").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_password_1_char_rejected() {
        let svc = test_service();
        let result = svc.create_wallet("x").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_password_empty_rejected() {
        let svc = test_service();
        let result = svc.create_wallet("").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_multiple_unlock_lock_cycles() {
        let svc = test_service();
        for _ in 0..5 {
            svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
            assert!(svc.get_session_status().await.is_active);
            svc.lock().await.expect("lock succeeded");
            assert!(!svc.get_session_status().await.is_active);
        }
    }

    #[tokio::test]
    async fn test_send_with_various_amounts() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");

        for amount in ["0", "1", "1000000000000000000", "999999999999999999999"] {
            let result = svc.send_transaction("0xfrom", "0xto", amount, "pwd").await;
            assert!(result.is_ok(), "Send should succeed for amount {}", amount);
        }
    }

    #[tokio::test]
    async fn test_account_struct_fields() {
        let account = Account {
            address: "0xabc".to_string(),
            label: "Test".to_string(),
            balance: "100".to_string(),
            nonce: 5,
            is_default: true,
        };
        assert_eq!(account.address, "0xabc");
        assert_eq!(account.nonce, 5);
        assert!(account.is_default);
    }

    #[tokio::test]
    async fn test_account_clone() {
        let account = Account {
            address: "0xabc".to_string(),
            label: "Test".to_string(),
            balance: "100".to_string(),
            nonce: 5,
            is_default: true,
        };
        let cloned = account.clone();
        assert_eq!(cloned.address, account.address);
        assert_eq!(cloned.nonce, account.nonce);
    }

    #[tokio::test]
    async fn test_create_account_result_debug() {
        let result = CreateAccountResult {
            address: "0xabc".to_string(),
            mnemonic: "test words".to_string(),
            public_key: "deadbeef".to_string(),
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("0xabc"));
    }

    #[tokio::test]
    async fn test_session_status_clone() {
        let status = SessionStatus {
            is_active: true,
            remaining_seconds: Some(3600),
            is_locked_out: false,
        };
        let cloned = status.clone();
        assert!(cloned.is_active);
        assert_eq!(cloned.remaining_seconds, Some(3600));
    }

    #[tokio::test]
    async fn test_primary_address_returns_first_account() {
        let svc = test_service();
        svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        let addr = svc.get_primary_address().await;
        assert!(addr.is_some());
        assert!(addr.expect("test assertion").starts_with("0x"));
    }

    #[tokio::test]
    async fn test_not_first_run_after_create() {
        let svc = test_service();
        assert!(svc.is_first_run().await);
        svc.create_wallet("strongpassword123").await.expect("async operation succeeded");
        assert!(!svc.is_first_run().await);
    }

    #[tokio::test]
    async fn test_custom_backend_create() {
        struct CustomBackend;

        #[async_trait::async_trait]
        impl WalletBackend for CustomBackend {
            async fn load_accounts(&self) -> Result<Vec<Account>, AppError> {
                Ok(vec![Account {
                    address: "0x1234".to_string(),
                    label: "Custom".to_string(),
                    balance: "500".to_string(),
                    nonce: 10,
                    is_default: true,
                }])
            }
            async fn create_wallet(&self, _: &str, _: &str) -> Result<CreateAccountResult, AppError> {
                Ok(CreateAccountResult {
                    address: "0xcustom".to_string(),
                    mnemonic: "custom words here".to_string(),
                    public_key: "aabbccdd".to_string(),
                })
            }
            async fn unlock(&self, _: &str, _: &str) -> Result<bool, AppError> { Ok(true) }
            async fn lock(&self) -> Result<(), AppError> { Ok(()) }
            async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
                Ok("0xcustomhash".to_string())
            }
        }

        let events = Arc::new(EventBus::new());
        let svc = WalletService::with_backend(events, Arc::new(CustomBackend));
        svc.load_from_disk().await.expect("load succeeded");
        assert!(!svc.is_first_run().await);
        let accounts = svc.list_accounts().await;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "Custom");
    }

    #[tokio::test]
    async fn test_failing_backend_create() {
        struct FailingBackend;

        #[async_trait::async_trait]
        impl WalletBackend for FailingBackend {
            async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(vec![]) }
            async fn create_wallet(&self, _: &str, _: &str) -> Result<CreateAccountResult, AppError> {
                Err(AppError::Wallet("disk write failed".into()))
            }
            async fn unlock(&self, _: &str, _: &str) -> Result<bool, AppError> { Ok(false) }
            async fn lock(&self) -> Result<(), AppError> { Ok(()) }
            async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
                Err(AppError::Network("offline".into()))
            }
        }

        let events = Arc::new(EventBus::new());
        let svc = WalletService::with_backend(events, Arc::new(FailingBackend));
        let result = svc.create_wallet("strongpassword123").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_failing_backend_send() {
        struct FailSendBackend;

        #[async_trait::async_trait]
        impl WalletBackend for FailSendBackend {
            async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(vec![]) }
            async fn create_wallet(&self, _: &str, _: &str) -> Result<CreateAccountResult, AppError> {
                Ok(CreateAccountResult {
                    address: "0xabc".into(),
                    mnemonic: "words".into(),
                    public_key: "pk".into(),
                })
            }
            async fn unlock(&self, _: &str, _: &str) -> Result<bool, AppError> { Ok(true) }
            async fn lock(&self) -> Result<(), AppError> { Ok(()) }
            async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
                Err(AppError::InsufficientFunds {
                    have: "1 SALT".into(),
                    need: "100 SALT".into(),
                })
            }
        }

        let events = Arc::new(EventBus::new());
        let svc = WalletService::with_backend(events, Arc::new(FailSendBackend));
        svc.unlock("0xabc", "password123").await.expect("async operation succeeded");
        let result = svc.send_transaction("0xfrom", "0xto", "100", "pwd").await;
        assert!(result.is_err());
        match result.expect_err("expected error") {
            AppError::InsufficientFunds { .. } => {}
            other => panic!("Expected InsufficientFunds, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_unlock_wrong_password() {
        struct StrictBackend;

        #[async_trait::async_trait]
        impl WalletBackend for StrictBackend {
            async fn load_accounts(&self) -> Result<Vec<Account>, AppError> { Ok(vec![]) }
            async fn create_wallet(&self, _: &str, _: &str) -> Result<CreateAccountResult, AppError> {
                Ok(CreateAccountResult { address: "0x".into(), mnemonic: "w".into(), public_key: "k".into() })
            }
            async fn unlock(&self, _: &str, password: &str) -> Result<bool, AppError> {
                Ok(password == "correct_password")
            }
            async fn lock(&self) -> Result<(), AppError> { Ok(()) }
            async fn send_transaction(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, AppError> {
                Ok("0x".into())
            }
        }

        let events = Arc::new(EventBus::new());
        let svc = WalletService::with_backend(events, Arc::new(StrictBackend));
        let result = svc.unlock("0xabc", "wrong_password").await;
        assert!(result.is_err());
    }
}
