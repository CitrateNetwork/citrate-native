//! Wallet operations service.
//!
//! Data source: wallet/ crate (ed25519 keypair management, transaction signing)
//! This service wraps the wallet manager and exposes typed operations
//! that the UI layer consumes directly.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use citrate_wallet_core::{PersistedFailure, SessionManager};
use std::path::PathBuf;
use std::sync::{Arc, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;
use tokio::sync::RwLock;

/// Session timeout: 1 hour. After this many seconds without activity
/// the wallet auto-locks and `is_session_active` returns false.
/// RM-B1 / WP-E2.1 (audit GUI-C-01).
const SESSION_TIMEOUT_SECS: u64 = 3600;

/// Lockout policy: 5 wrong-password attempts triggers a 5-minute
/// lockout. Matches the planset's "5 attempts / 5-min cooldown."
/// RM-B1 / WP-E2.2 (audit GUI-C-02, WAL-09).
const MAX_FAILED_ATTEMPTS: u32 = 5;
const LOCKOUT_DURATION_SECS: u64 = 300;

/// Re-auth value threshold: any send of `>=` this many wei requires
/// the user to have entered their password within `RE_AUTH_FRESHNESS_SECS`.
/// 10 SALT (10 * 10^18 wei) per the audit recommendation.
/// RM-B1 / WP-E2.5 (audit WAL-07).
pub const RE_AUTH_THRESHOLD_WEI: u128 = 10_000_000_000_000_000_000u128;

/// How recent the password entry must be for high-value sends.
/// 60 seconds is short enough to defeat session-cookie capture from
/// a malicious tab while not being so short it kills usability.
pub const RE_AUTH_FRESHNESS_SECS: u64 = 60;

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
    /// Sign and send a transaction with calldata (for contract/precompile calls)
    async fn send_transaction_with_data(&self, from: &str, to: &str, value_wei: &str, _data: Vec<u8>, password: &str) -> Result<String, AppError> {
        // Default: ignore data, fall back to value-only send
        self.send_transaction(from, to, value_wei, password).await
    }
    /// Update signing chain ID for environment switching
    fn set_chain_id(&self, _chain_id: u64) {}
    /// Update RPC URL target for environment switching
    fn set_rpc_url(&self, _url: &str) {}
}

/// Production wallet backend — delegates to citrate-wallet-core for real
/// Argon2+AES-GCM key management, BIP39 mnemonics, and transaction signing.
pub struct WalletCoreBackend {
    key_manager: Arc<citrate_wallet_core::KeyManager>,
    rpc_client: std::sync::RwLock<Arc<citrate_wallet_core::RpcClient>>,
    /// Chain ID for transaction signing — derived from AppConfig, not hardcoded.
    chain_id: std::sync::atomic::AtomicU64,
}

impl WalletCoreBackend {
    pub fn new() -> Self {
        let config = citrate_wallet_core::WalletConfig::default();
        let keystore_path = std::path::PathBuf::from(&config.keystore_path);
        Self {
            key_manager: Arc::new(citrate_wallet_core::KeyManager::new(&keystore_path)),
            rpc_client: std::sync::RwLock::new(Arc::new(citrate_wallet_core::RpcClient::new(&config.rpc_url))),
            chain_id: std::sync::atomic::AtomicU64::new(40204), // default testnet
        }
    }

    /// Update the signing chain ID (called when environment switches).
    pub fn set_chain_id(&self, id: u64) {
        self.chain_id.store(id, std::sync::atomic::Ordering::Relaxed);
    }

    /// Update the RPC URL target (called when environment switches).
    pub fn set_rpc_url(&self, url: &str) {
        let new_client = Arc::new(citrate_wallet_core::RpcClient::new(url));
        *self.rpc_client_write() = new_client;
        tracing::info!("WalletCoreBackend: RPC target updated to {}", url);
    }

    fn get_chain_id(&self) -> u64 {
        self.chain_id.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn rpc_client_read(&self) -> RwLockReadGuard<'_, Arc<citrate_wallet_core::RpcClient>> {
        match self.rpc_client.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn rpc_client_write(&self) -> RwLockWriteGuard<'_, Arc<citrate_wallet_core::RpcClient>> {
        match self.rpc_client.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
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

        // Clone the RPC client Arc so we don't hold the lock across await points
        let rpc = self.rpc_client_read().clone();

        // Get the nonce from the chain — MUST succeed, no fallback to 0
        let nonce = rpc.get_nonce(from).await
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
                    .chain_id(self.get_chain_id())
                    .sign(ed_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("Ed25519 sign failed: {}", e)))?
            }
            citrate_wallet_core::keys::UnifiedKey::Secp256k1(secp_key) => {
                citrate_wallet_core::TransactionBuilder::new()
                    .to(to)
                    .value(value)
                    .chain_id(self.get_chain_id())
                    .sign_secp256k1(secp_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("secp256k1 sign failed: {}", e)))?
            }
        };

        // Submit to RPC — MUST succeed, no local hash fallback
        // F-03 fix: failed submission is a real error, not a fake success
        let tx_hash = rpc.send_raw_transaction(&signed.raw).await
            .map_err(|e| AppError::Network(format!("Transaction submission failed: {}. The transaction was signed but not accepted by the network.", e)))?;

        Ok(tx_hash)
    }

    async fn send_transaction_with_data(&self, from: &str, to: &str, value_wei: &str, data: Vec<u8>, _password: &str) -> Result<String, AppError> {
        let unified_key = self.key_manager.get_signing_key(from)
            .map_err(|e| AppError::Wallet(format!("Cannot sign: {}", e)))?;

        let rpc = self.rpc_client_read().clone();
        let nonce = rpc.get_nonce(from).await
            .map_err(|e| AppError::Network(format!("Cannot fetch nonce: {}", e)))?;

        let value: u128 = value_wei.parse()
            .map_err(|_| AppError::Wallet(format!("Invalid amount: '{}'", value_wei)))?;

        let signed = match &unified_key {
            citrate_wallet_core::keys::UnifiedKey::Ed25519(ed_key) => {
                citrate_wallet_core::TransactionBuilder::new()
                    .to(to)
                    .value(value)
                    .data(data)
                    .chain_id(self.get_chain_id())
                    .sign(ed_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("Ed25519 sign failed: {}", e)))?
            }
            citrate_wallet_core::keys::UnifiedKey::Secp256k1(secp_key) => {
                citrate_wallet_core::TransactionBuilder::new()
                    .to(to)
                    .value(value)
                    .data(data)
                    .chain_id(self.get_chain_id())
                    .sign_secp256k1(secp_key, nonce)
                    .map_err(|e| AppError::Wallet(format!("secp256k1 sign failed: {}", e)))?
            }
        };

        let tx_hash = rpc.send_raw_transaction(&signed.raw).await
            .map_err(|e| AppError::Network(format!("Transaction failed: {}", e)))?;

        Ok(tx_hash)
    }

    fn set_chain_id(&self, chain_id: u64) {
        self.chain_id.store(chain_id, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("WalletCoreBackend: chain_id updated to {}", chain_id);
    }

    fn set_rpc_url(&self, url: &str) {
        let new_client = Arc::new(citrate_wallet_core::RpcClient::new(url));
        *self.rpc_client_write() = new_client;
        tracing::info!("WalletCoreBackend: RPC target updated to {}", url);
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

/// Session status for the wallet lock/unlock lifecycle.
///
/// RM-B1 / WP-E2.1 (audit GUI-C-01): pre-fix `remaining_seconds`
/// was a static `Some(3600)` not tied to time at all. Post-fix the
/// value is computed from `SessionManager::get_status` against the
/// real Instant of last activity.
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub is_active: bool,
    pub remaining_seconds: Option<u64>,
    pub is_locked_out: bool,
    /// Seconds remaining on a brute-force lockout, if any.
    /// RM-B1 / WP-E2.2 (audit GUI-C-02, WAL-09).
    pub lockout_remaining_seconds: Option<u64>,
}

/// Wallet operations service.
pub struct WalletService {
    events: Arc<EventBus>,
    accounts: Arc<RwLock<Vec<Account>>>,
    /// RM-B1 / WP-E2.2 (audit GUI-C-02, WAL-09): authoritative
    /// session + lockout state. The previous `Arc<RwLock<SessionStatus>>`
    /// was a cosmetic snapshot — `is_active` flipped to true on unlock
    /// and stayed true until manual lock(), with no time-based
    /// expiration.
    session_mgr: Arc<RwLock<SessionManager>>,
    /// Address whose unlock currently owns the session, if any.
    /// Used so `is_session_active` can resolve without an explicit
    /// argument from callers and so per-account semantics are
    /// possible.
    active_address: Arc<RwLock<Option<String>>>,
    /// When the user most recently proved possession of the password.
    /// `Some(Instant)` after a successful unlock, `None` after lock or
    /// before first unlock. Used by the re-auth threshold check.
    /// RM-B1 / WP-E2.5 (audit WAL-07).
    last_unlock_at: Arc<RwLock<Option<Instant>>>,
    /// On-disk path for lockout-state persistence. `None` keeps the
    /// state in memory only (used by tests and headless contexts).
    /// RM-B1 / WP-E2.3 (audit WAL-05).
    lockout_file: Arc<std::sync::RwLock<Option<PathBuf>>>,
    backend: Arc<dyn WalletBackend>,
    /// Current RPC URL, kept in sync with the backend so callers that
    /// need to talk to the SAME node the wallet submits to (e.g. receipt
    /// polling after `send_transaction_with_data`) have a canonical
    /// source. Updated by `set_rpc_url`.
    rpc_url: Arc<std::sync::RwLock<String>>,
}

fn new_session_manager() -> SessionManager {
    SessionManager::new(MAX_FAILED_ATTEMPTS, LOCKOUT_DURATION_SECS, SESSION_TIMEOUT_SECS)
}

impl WalletService {
    /// Create with the embedded wallet backend (production).
    pub fn new(events: Arc<EventBus>) -> Self {
        let initial_url = citrate_wallet_core::WalletConfig::default().rpc_url;
        Self {
            events,
            accounts: Arc::new(RwLock::new(Vec::new())),
            session_mgr: Arc::new(RwLock::new(new_session_manager())),
            active_address: Arc::new(RwLock::new(None)),
            last_unlock_at: Arc::new(RwLock::new(None)),
            lockout_file: Arc::new(std::sync::RwLock::new(None)),
            backend: Arc::new(WalletCoreBackend::new()),
            rpc_url: Arc::new(std::sync::RwLock::new(initial_url)),
        }
    }

    /// Create with an injected backend (for testing or alternative wallet).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn WalletBackend>) -> Self {
        let initial_url = citrate_wallet_core::WalletConfig::default().rpc_url;
        Self {
            events,
            accounts: Arc::new(RwLock::new(Vec::new())),
            session_mgr: Arc::new(RwLock::new(new_session_manager())),
            active_address: Arc::new(RwLock::new(None)),
            last_unlock_at: Arc::new(RwLock::new(None)),
            lockout_file: Arc::new(std::sync::RwLock::new(None)),
            backend,
            rpc_url: Arc::new(std::sync::RwLock::new(initial_url)),
        }
    }

    /// Configure on-disk persistence of the brute-force lockout state.
    /// Existing state is loaded synchronously; subsequent `unlock`
    /// failures persist back to the same path with mode 0600 on Unix.
    /// RM-B1 / WP-E2.3 (audit WAL-05).
    pub async fn enable_lockout_persistence(&self, path: PathBuf) {
        // Load any prior state.
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(items) = serde_json::from_slice::<Vec<PersistedFailure>>(&bytes) {
                self.session_mgr.write().await.import_failures(items);
            } else {
                tracing::warn!(
                    "WalletService: lockout file at {} corrupt, ignoring",
                    path.display()
                );
            }
        }
        if let Ok(mut guard) = self.lockout_file.write() {
            *guard = Some(path);
        }
    }

    /// Persist lockout state to disk if `enable_lockout_persistence`
    /// has set a path. No-op otherwise.
    async fn persist_lockout_state(&self) {
        let path_opt = self
            .lockout_file
            .read()
            .ok()
            .and_then(|g| g.clone());
        let Some(path) = path_opt else {
            return;
        };
        let snapshot = self.session_mgr.read().await.export_failures();
        let bytes = match serde_json::to_vec_pretty(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("WalletService: failed to serialize lockout state: {}", e);
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, &bytes) {
            tracing::warn!(
                "WalletService: failed to persist lockout state to {}: {}",
                path.display(),
                e
            );
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }

    /// True iff the given address currently has an active (non-expired,
    /// non-locked-out) session. Per-account: locking 0xA does not
    /// affect 0xB's session state.
    /// RM-B1 / WP-E2.6 (audit WAL-08).
    pub async fn is_account_unlocked(&self, address: &str) -> bool {
        let mgr = self.session_mgr.read().await;
        mgr.is_session_active(address)
    }

    /// Resolve a SessionStatus snapshot from the session manager.
    async fn current_status(&self) -> SessionStatus {
        let active_addr = self.active_address.read().await.clone();
        match active_addr {
            Some(addr) => {
                let mgr = self.session_mgr.read().await;
                let core_status = mgr.get_status(&addr);
                SessionStatus {
                    is_active: core_status.is_active,
                    remaining_seconds: core_status.remaining_secs,
                    is_locked_out: core_status.is_locked_out,
                    lockout_remaining_seconds: core_status.lockout_remaining_secs,
                }
            }
            None => SessionStatus {
                is_active: false,
                remaining_seconds: None,
                is_locked_out: false,
                lockout_remaining_seconds: None,
            },
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

        // Activate the session immediately. The user just typed this
        // password to encrypt the keystore — making them re-type it to
        // sign the first tx is the source of "Send failed: Session
        // expired" right after onboarding. Same convention as a fresh
        // browser-wallet install: create → unlocked.
        *self.active_address.write().await = Some(result.address.clone());
        *self.last_unlock_at.write().await = Some(Instant::now());
        self.session_mgr.write().await.record_success(&result.address);

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

        // Activate the session for the same reason create_wallet does:
        // the password we just validated by decrypting the keystore is
        // sufficient — don't make the user re-type it for the next op.
        *self.active_address.write().await = Some(result.address.clone());
        *self.last_unlock_at.write().await = Some(Instant::now());
        self.session_mgr.write().await.record_success(&result.address);

        Ok(result)
    }

    /// Unlock the wallet with password.
    ///
    /// RM-B1 / WP-E2.2 (audit GUI-C-02, WAL-09): the lockout counter
    /// is incremented for the address that was *attempted*, not for
    /// some always-primary handle. Pre-fix the GUI tracked failed
    /// attempts on whatever was active rather than the password's
    /// claimed identity, so an attacker could brute-force account A
    /// while the GUI logged failures against account B (or none).
    pub async fn unlock(&self, address: &str, password: &str) -> Result<SessionStatus, AppError> {
        if password.is_empty() {
            return Err(AppError::Wallet("Password required".to_string()));
        }

        // RM-B1 / WP-E2.2 (audit GUI-C-02): refuse to even attempt
        // an unlock on a locked-out address.
        {
            let mgr = self.session_mgr.read().await;
            if mgr.is_locked_out(address) {
                return Err(AppError::Wallet(format!(
                    "Account {} is locked out due to too many failed attempts. Try again later.",
                    address
                )));
            }
        }

        let valid = match self.backend.unlock(address, password).await {
            Ok(v) => v,
            Err(e) => {
                // The backend itself errored (not a wrong-password
                // signal). Don't treat this as a brute-force attempt
                // — surface the underlying failure verbatim.
                return Err(e);
            }
        };

        if !valid {
            // Wrong password → record against the *attempted* address.
            {
                let mut mgr = self.session_mgr.write().await;
                let _ = mgr.record_failure(address);
            }
            // Persist updated counter so a process restart cannot
            // launder away the failure.
            // RM-B1 / WP-E2.3 (audit WAL-05).
            self.persist_lockout_state().await;
            return Err(AppError::Wallet("Invalid password".to_string()));
        }

        // Success — reset failure counter, mint a fresh session.
        *self.active_address.write().await = Some(address.to_string());
        *self.last_unlock_at.write().await = Some(Instant::now());
        self.session_mgr.write().await.record_success(address);
        // Failure counter cleared on success; persist that too.
        self.persist_lockout_state().await;
        Ok(self.current_status().await)
    }

    /// Lock the wallet
    pub async fn lock(&self) -> Result<(), AppError> {
        self.backend.lock().await?;
        // End all sessions in the manager. Failure counters survive
        // (a deliberate lock should NOT reset brute-force attempts).
        self.session_mgr.write().await.end_all_sessions();
        *self.active_address.write().await = None;
        *self.last_unlock_at.write().await = None;
        Ok(())
    }

    /// Send SALT between accounts
    /// RM-B1 / WP-E2.5 (audit WAL-07): re-auth above the
    /// `RE_AUTH_THRESHOLD_WEI` (10 SALT). A long-lived session is fine for
    /// low-value sends, but high-value transfers must require the user to
    /// have proved password possession recently.
    ///
    /// RM-B / GUI_NATIVE-001: hoisted into a single helper so EVERY signing
    /// entry point (plain + calldata-bearing) enforces the threshold. A new
    /// signing method that forgets to call this is the exact regression the
    /// `test_wal07_*` tripwires guard against — keep the call at the top of
    /// each signing path.
    async fn enforce_value_reauth(&self, value_wei: &str) -> Result<(), AppError> {
        if let Ok(value) = value_wei.parse::<u128>() {
            if value >= RE_AUTH_THRESHOLD_WEI {
                let last_unlock = *self.last_unlock_at.read().await;
                let stale = match last_unlock {
                    Some(when) => when.elapsed().as_secs() >= RE_AUTH_FRESHNESS_SECS,
                    None => true,
                };
                if stale {
                    return Err(AppError::Wallet(format!(
                        "Re-authentication required: transfers above {} wei require password entry within the last {} seconds",
                        RE_AUTH_THRESHOLD_WEI, RE_AUTH_FRESHNESS_SECS
                    )));
                }
            }
        }
        Ok(())
    }

    pub async fn send_transaction(
        &self,
        from: &str,
        to: &str,
        value_wei: &str,
        password: &str,
    ) -> Result<String, AppError> {
        // RM-B1 / WP-E2.4 (audit WAL-06): pre-sign session check.
        // The session manager's `is_session_active` is real (Instant-
        // backed), so a session that has timed out since the last
        // user action is correctly refused here.
        let status = self.current_status().await;
        if !status.is_active {
            return Err(AppError::SessionExpired);
        }

        // RM-B1 / WP-E2.5 (audit WAL-07): high-value re-auth threshold.
        self.enforce_value_reauth(value_wei).await?;

        // Touch the session so this signing op resets the inactivity
        // timer.
        if let Some(addr) = self.active_address.read().await.clone() {
            self.session_mgr.write().await.touch_session(&addr);
        }

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
        self.current_status().await
    }

    /// Send a transaction with calldata (for contract/precompile interaction).
    /// Data source: transaction sent to specified address with ABI-encoded calldata.
    pub async fn send_transaction_with_data(
        &self,
        from: &str,
        to: &str,
        value_wei: &str,
        data: Vec<u8>,
        password: &str,
    ) -> Result<String, AppError> {
        // RM-B1 / WP-E2.4 (audit WAL-06): pre-sign session check.
        let status = self.current_status().await;
        if !status.is_active {
            return Err(AppError::SessionExpired);
        }

        // RM-B / GUI_NATIVE-001 (audit WAL-07 bypass fix): the calldata
        // path honors `value_wei` (staking joinPool/registerProvider send
        // 1000 SALT), so it must enforce the SAME high-value re-auth
        // threshold as `send_transaction`. Previously absent — a stale
        // session could authorize a 100x-threshold value-bearing call.
        self.enforce_value_reauth(value_wei).await?;

        if let Some(addr) = self.active_address.read().await.clone() {
            self.session_mgr.write().await.touch_session(&addr);
        }

        let tx_hash = self.backend.send_transaction_with_data(from, to, value_wei, data, password).await?;

        self.events.publish(AppEvent::TransactionConfirmed {
            tx_hash: tx_hash.clone(),
            block_height: 0,
            success: true,
        });

        Ok(tx_hash)
    }

    /// Update the signing chain ID — called on environment switch.
    pub fn set_chain_id(&self, chain_id: u64) {
        self.backend.set_chain_id(chain_id);
    }

    /// Update the RPC URL target — called on environment switch.
    pub fn set_rpc_url(&self, url: &str) {
        self.backend.set_rpc_url(url);
        if let Ok(mut guard) = self.rpc_url.write() {
            *guard = url.to_string();
        }
    }

    /// Current RPC URL the wallet is submitting transactions to. Use this
    /// for receipt polling so the poll targets the SAME node as the
    /// submission — mismatched URLs cause "rpc transport: error sending
    /// request" because the receipt lives on the submission node, not
    /// whatever arbitrary port the caller assumed.
    pub fn get_rpc_url(&self) -> String {
        self.rpc_url
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
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

    /// RM-B1 / WP-E2.1 (audit GUI-C-01): real session decrement.
    /// Pre-fix `remaining_seconds` was a static `Some(3600)` value
    /// not tied to time. Post-fix it's computed from a real Instant
    /// and decreases as wall-clock time passes.
    #[tokio::test]
    async fn test_session_decrements_then_locks() {
        let svc = test_service();
        let status = svc
            .unlock("0xabc", "password123")
            .await
            .expect("unlock");
        let initial = status.remaining_seconds.expect("initial seconds");
        assert!(initial <= SESSION_TIMEOUT_SECS && initial > 0);

        // Force the session timer to expire by reaching into the
        // SessionManager and ending the session — the visible API
        // we have for "fast-forward" without sleeping for an hour.
        svc.session_mgr.write().await.end_all_sessions();

        let after = svc.get_session_status().await;
        assert!(!after.is_active, "session must be inactive after end");
        assert!(
            after.remaining_seconds.is_none(),
            "remaining_seconds is None when locked"
        );
    }

    /// RM-B1 / WP-E2.2 (audit GUI-C-02, WAL-09): lockout fires at
    /// MAX_FAILED_ATTEMPTS attempts, recorded against the address
    /// that was tried.
    #[tokio::test]
    async fn test_lockout_after_repeated_wrong_passwords() {
        let svc = test_service();
        let target = "0xvictim";

        // The TestWalletBackend rejects any password except "password123".
        for _ in 0..(MAX_FAILED_ATTEMPTS - 1) {
            // The TestWalletBackend rejects passwords <8 chars.
            let _ = svc.unlock(target, "bad").await;
        }
        let final_attempt = svc.unlock(target, "bad").await;
        assert!(final_attempt.is_err(), "attempt 5 must fail");

        // After lockout, even the correct password is rejected with
        // a lockout message.
        let after = svc.unlock(target, "password123").await;
        match after {
            Err(AppError::Wallet(msg)) => {
                assert!(
                    msg.contains("locked out"),
                    "expected lockout message, got: {}",
                    msg
                );
            }
            other => panic!("expected lockout error, got: {:?}", other),
        }
    }

    /// RM-B1 / WP-E2.2: failures recorded against the *attempted*
    /// address don't lock out a different account.
    #[tokio::test]
    async fn test_lockout_is_per_address() {
        let svc = test_service();
        for _ in 0..MAX_FAILED_ATTEMPTS {
            let _ = svc.unlock("0xvictim", "bad").await;
        }
        // 0xother is unaffected.
        let ok = svc.unlock("0xother", "password123").await;
        assert!(ok.is_ok(), "other address must remain unlockable");
    }

    /// RM-B1 / WP-E2.3 (audit WAL-05): persisted lockout state
    /// survives a "process restart" (constructing a fresh
    /// WalletService pointing at the same file).
    #[tokio::test]
    async fn test_wal05_lockout_persists_across_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("lockout.json");

        let events = Arc::new(EventBus::new());
        let svc1 = WalletService::with_backend(events, Arc::new(TestWalletBackend));
        svc1.enable_lockout_persistence(path.clone()).await;

        // 4 failed attempts (one short of lockout).
        for _ in 0..(MAX_FAILED_ATTEMPTS - 1) {
            let _ = svc1.unlock("0xvictim", "bad").await;
        }

        // Fresh service pointing at the same file.
        let events2 = Arc::new(EventBus::new());
        let svc2 = WalletService::with_backend(events2, Arc::new(TestWalletBackend));
        svc2.enable_lockout_persistence(path.clone()).await;

        // One more attempt → lockout should fire even though this
        // is the first attempt the new service sees.
        let res = svc2.unlock("0xvictim", "bad").await;
        assert!(res.is_err());

        let after = svc2.unlock("0xvictim", "password123").await;
        match after {
            Err(AppError::Wallet(msg)) => {
                assert!(msg.contains("locked out"), "msg = {}", msg);
            }
            other => panic!("expected lockout error, got: {:?}", other),
        }
    }

    /// RM-B1 / WP-E2.5 (audit WAL-07): high-value send requires
    /// recent password entry.
    #[tokio::test]
    async fn test_wal07_high_value_send_requires_fresh_unlock() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("unlock");

        // Sub-threshold send works.
        let small_value = (RE_AUTH_THRESHOLD_WEI / 2).to_string();
        let r = svc
            .send_transaction("0xabc", "0xto", &small_value, "password123")
            .await;
        assert!(r.is_ok(), "sub-threshold send must work");

        // Force the unlock timestamp into the past, beyond freshness.
        {
            let mut last = svc.last_unlock_at.write().await;
            *last = Some(
                Instant::now()
                    - std::time::Duration::from_secs(RE_AUTH_FRESHNESS_SECS + 5),
            );
        }

        // Above-threshold send must require re-auth.
        let big_value = RE_AUTH_THRESHOLD_WEI.to_string();
        let r = svc
            .send_transaction("0xabc", "0xto", &big_value, "password123")
            .await;
        match r {
            Err(AppError::Wallet(msg)) => {
                assert!(msg.contains("Re-authentication"), "msg = {}", msg);
            }
            other => panic!("expected re-auth error, got {:?}", other),
        }

        // Re-unlock refreshes the timestamp; high-value send works.
        svc.unlock("0xabc", "password123").await.expect("re-unlock");
        let r = svc
            .send_transaction("0xabc", "0xto", &big_value, "password123")
            .await;
        assert!(r.is_ok(), "high-value send works after fresh unlock");
    }

    /// RM-B / GUI_NATIVE-001 (audit WAL-07 bypass): the calldata-bearing
    /// signing path MUST enforce the same high-value re-auth threshold as
    /// the plain `send_transaction`. Pre-fix this returned Ok for a
    /// 100x-threshold staking call on a stale session (the bypass).
    #[tokio::test]
    async fn test_wal07_with_data_enforces_reauth() {
        let svc = test_service();
        svc.unlock("0xabc", "password123").await.expect("unlock");

        // Sub-threshold calldata send works.
        let small_value = (RE_AUTH_THRESHOLD_WEI / 2).to_string();
        let r = svc
            .send_transaction_with_data("0xabc", "0xpool", &small_value, vec![0u8; 36], "password123")
            .await;
        assert!(r.is_ok(), "sub-threshold calldata send must work");

        // Age the unlock beyond the freshness window.
        {
            let mut last = svc.last_unlock_at.write().await;
            *last = Some(
                Instant::now()
                    - std::time::Duration::from_secs(RE_AUTH_FRESHNESS_SECS + 5),
            );
        }

        // 1000-SALT (100x threshold) staking call via the calldata path
        // must now require re-auth (this is the bug being closed).
        let big_value = (RE_AUTH_THRESHOLD_WEI * 100).to_string();
        let r = svc
            .send_transaction_with_data("0xabc", "0xpool", &big_value, vec![0u8; 36], "password123")
            .await;
        match r {
            Err(AppError::Wallet(msg)) => {
                assert!(msg.contains("Re-authentication"), "msg = {}", msg);
            }
            other => panic!("expected re-auth error on calldata path, got {:?}", other),
        }

        // Fresh unlock re-enables the high-value calldata send.
        svc.unlock("0xabc", "password123").await.expect("re-unlock");
        let r = svc
            .send_transaction_with_data("0xabc", "0xpool", &big_value, vec![0u8; 36], "password123")
            .await;
        assert!(r.is_ok(), "high-value calldata send works after fresh unlock");
    }

    /// RM-B1 / WP-E2.6 (audit WAL-08): per-account session view.
    /// `is_account_unlocked` reflects per-address state independently
    /// of which account is "primary."
    #[tokio::test]
    async fn test_wal08_per_account_unlock_status() {
        let svc = test_service();
        svc.unlock("0xalice", "password123").await.expect("unlock alice");

        assert!(svc.is_account_unlocked("0xalice").await);
        assert!(!svc.is_account_unlocked("0xbob").await);

        // Unlock bob too.
        svc.unlock("0xbob", "password123").await.expect("unlock bob");
        assert!(svc.is_account_unlocked("0xalice").await);
        assert!(svc.is_account_unlocked("0xbob").await);

        // Lock everything; both go inactive.
        svc.lock().await.expect("lock");
        assert!(!svc.is_account_unlocked("0xalice").await);
        assert!(!svc.is_account_unlocked("0xbob").await);
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
            lockout_remaining_seconds: None,
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
