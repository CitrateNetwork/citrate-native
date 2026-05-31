//! Node lifecycle and chain query service.
//!
//! Data source: citrate-consensus, citrate-storage, citrate-execution, citrate-api
//! via the embedded node manager. Direct Rust calls — no loopback RPC.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::AppConfig;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Live node status snapshot
#[derive(Debug, Clone)]
pub struct NodeStatus {
    pub running: bool,
    pub block_height: u64,
    pub peer_count: u32,
    pub mempool_size: usize,
    pub dag_tips: usize,
    pub syncing: bool,
    pub chain_id: u64,
    pub uptime_seconds: u64,
}

impl Default for NodeStatus {
    fn default() -> Self {
        Self {
            running: false,
            block_height: 0,
            peer_count: 0,
            mempool_size: 0,
            dag_tips: 0,
            syncing: false,
            chain_id: 40204,
            uptime_seconds: 0,
        }
    }
}

/// Block summary for explorer display
#[derive(Debug, Clone)]
pub struct BlockSummary {
    pub hash: String,
    pub height: u64,
    pub timestamp: u64,
    pub tx_count: usize,
    pub selected_parent: String,
    pub blue_score: u64,
    /// T2-15: who proposed this block. Full 64-char hex of the
    /// ed25519 proposer pubkey. UI truncates for display.
    pub proposer: String,
}

/// Trait for real node backend implementations.
/// The GUI binary provides the real implementation; tests use the default stub.
#[async_trait::async_trait]
pub trait NodeBackend: Send + Sync {
    async fn start_node(&self, chain_id: u64, data_dir: &str) -> Result<(), AppError>;
    async fn stop_node(&self) -> Result<(), AppError>;
    async fn get_block_height(&self) -> u64;
    async fn get_peer_count(&self) -> u32;
    async fn get_mempool_size(&self) -> usize;
    async fn get_balance(&self, address: &[u8; 20]) -> String;
    /// Read real block summaries from storage. Returns newest-first.
    async fn get_block_summaries(&self, _count: usize) -> Vec<BlockSummary> {
        Vec::new()
    }
    /// Update bootstrap nodes for network switching.
    async fn set_bootnodes(&self, _bootnodes: Vec<String>) {}
    /// Get transactions for an address from local storage.
    async fn get_transactions_for(&self, _address: &str, _limit: usize) -> Vec<TxSummary> {
        Vec::new()
    }
    /// Full transaction details for every tx in a block.
    ///
    /// `block_hash` is the `0x`-prefixed hex of the 32-byte block hash. Returns
    /// an empty vec if the block is not in local storage. Backends that don't
    /// hold receipts can return entries with `status = TxStatus::ReceiptMissing`.
    async fn get_block_transactions(&self, _block_hash: &str) -> Vec<BlockTxDetail> {
        Vec::new()
    }
}

/// Transaction summary for GUI display
#[derive(Clone, Debug)]
pub struct TxSummary {
    pub hash: String,
    pub tx_type: String,
    pub amount: String,
    pub counterparty: String,
    pub status: String,
    pub timestamp: String,
}

/// Full transaction detail for DAG explorer modal.
///
/// Data sources: `Block.transactions` (RocksDB block store) for everything
/// except `status` + `gas_used` + `effective_gas_price`, which come from
/// `CF_RECEIPTS` via `TransactionStore::get_receipt`.
#[derive(Clone, Debug)]
pub struct BlockTxDetail {
    pub tx_hash: String,
    pub from: String,
    /// `None` for contract-creation transactions.
    pub to: Option<String>,
    pub value_wei: String,
    pub nonce: u64,
    pub gas_limit: u64,
    pub gas_price_wei: u64,
    pub gas_used: u64,
    pub effective_gas_price_wei: u64,
    pub status: TxStatus,
    /// Raw calldata, lower-case hex without `0x` prefix. Empty for value transfers.
    pub input_hex: String,
    pub block_height: u64,
    pub eth_tx_type: u8,
}

/// Receipt status of a transaction in a confirmed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    /// Receipt present, `status == true`.
    Confirmed,
    /// Receipt present, `status == false` (EVM revert / halt).
    Failed,
    /// Block present but receipt missing in storage. Treated as informational.
    ReceiptMissing,
}

impl TxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxStatus::Confirmed => "confirmed",
            TxStatus::Failed => "failed",
            TxStatus::ReceiptMissing => "receipt-missing",
        }
    }
}

/// Embedded node backend — connects to the real citrate node crates.
/// Initializes RocksDB storage, genesis state, Noise identity,
/// and connects to the bootnode via Noise_XX_25519_ChaChaPoly_SHA256.
pub struct EmbeddedNodeBackend {
    storage: tokio::sync::RwLock<Option<Arc<citrate_storage::StorageManager>>>,
    peer_manager: tokio::sync::RwLock<Option<Arc<citrate_network::PeerManager>>>,
    state_db: tokio::sync::RwLock<Option<Arc<citrate_execution::StateDB>>>,
    running: std::sync::atomic::AtomicBool,
    /// Cancellation token for background tasks — supports instant wakeup on shutdown
    shutdown_notify: Arc<tokio::sync::Notify>,
    /// Cancellation flag for background tasks (discovery, sync)
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// JoinHandles for background tasks — awaited on stop for clean shutdown
    background_tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Bootnodes from config (F-05 fix: not hardcoded)
    bootnodes: tokio::sync::RwLock<Vec<String>>,
    /// IPFS daemon manager — auto-starts with the node
    ipfs_daemon: tokio::sync::RwLock<Option<Arc<citrate_storage::ipfs::IpfsDaemon>>>,
    /// Lifecycle mutex — prevents concurrent start/stop races that cause RocksDB LOCK errors.
    /// Held for the entire duration of start_node and stop_node.
    lifecycle: tokio::sync::Mutex<()>,
}

impl EmbeddedNodeBackend {
    pub fn new() -> Self {
        Self {
            storage: tokio::sync::RwLock::new(None),
            peer_manager: tokio::sync::RwLock::new(None),
            state_db: tokio::sync::RwLock::new(None),
            running: std::sync::atomic::AtomicBool::new(false),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            background_tasks: tokio::sync::Mutex::new(Vec::new()),
            // Current testnet-beta bootnodes (boot{1,2,3} discovery + sequencer
            // for sync). Hostnames resolve via citrate_network::resolve_bootnode.
            // Overridden by set_bootnodes() when the app config is loaded.
            bootnodes: tokio::sync::RwLock::new(vec![
                "noise_f356d3ebb07371eaad371b3960272f9d58fc457cde409c34549ef03776b78141@boot1.citrate.ai:30303".to_string(),
                "noise_4ed281386422f6a65b92d8760d24baa82bb1b476e9dd3e21d5a070d026802c07@boot2.citrate.ai:30303".to_string(),
                "noise_2b4924671e0babc9f52eb1695c72141a9c639e17ad95a2a2d2a715eae34a420e@boot3.citrate.ai:30303".to_string(),
                "noise_6ee549718d522c9ccc122585dfedef72aea8df24f4a2bb0264e7658a91118b4a@rpc.citrate.ai:30303".to_string(),
            ]),
            ipfs_daemon: tokio::sync::RwLock::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
        }
    }

}

impl Default for EmbeddedNodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedNodeBackend {
    /// Update bootnodes from config (F-05 fix: read from config, not hardcoded)
    pub async fn set_bootnodes(&self, bootnodes: Vec<String>) {
        *self.bootnodes.write().await = bootnodes;
    }
}

// Bootnode parsing + DNS resolution now lives in the shared
// `citrate_network::resolve_bootnode` so the embedded node resolves hostname
// bootnodes (e.g. boot1.citrate.ai) identically to the standalone daemon.

#[async_trait::async_trait]
impl NodeBackend for EmbeddedNodeBackend {
    async fn start_node(&self, chain_id: u64, data_dir: &str) -> Result<(), AppError> {
        use citrate_network::{
            NoiseKeypair, NetworkTransport, PeerManager, PeerManagerConfig,
            Discovery, DiscoveryConfig, transport::HandshakeParams,
        };
        use std::time::Duration;

        // Lifecycle mutex — prevents concurrent start/stop races that cause
        // RocksDB LOCK errors when the user clicks env-switch rapidly.
        let _lifecycle_guard = self.lifecycle.lock().await;

        // Prevent double-start
        if self.running.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::info!("Node already running — skipping start");
            return Ok(());
        }

        // Reset shutdown flag for fresh start
        self.shutdown.store(false, std::sync::atomic::Ordering::SeqCst);

        tracing::info!("Starting embedded node: chain_id={}, data_dir={}", chain_id, data_dir);

        // 1. Ensure data directory exists
        std::fs::create_dir_all(data_dir)
            .map_err(|e| AppError::Node(format!("Failed to create data dir: {}", e)))?;

        // Remove stale RocksDB LOCK file if present (from unclean shutdown / env switch)
        let lock_path = std::path::PathBuf::from(data_dir).join("LOCK");
        if lock_path.exists() {
            tracing::warn!("Removing stale RocksDB LOCK file at {:?}", lock_path);
            let _ = std::fs::remove_file(&lock_path);
        }

        // 2. Initialize RocksDB storage
        tracing::info!("Initializing RocksDB storage at {}", data_dir);
        let storage = Arc::new(citrate_storage::StorageManager::new(
            data_dir,
            citrate_storage::pruning::PruningConfig::default(),
        ).map_err(|e| AppError::Node(format!("Storage init failed: {}", e)))?);

        // 3. Initialize state DB and load existing state
        let state_db = Arc::new(citrate_execution::StateDB::new());
        match storage.state.get_all_accounts() {
            Ok(accounts) => {
                tracing::info!("Loaded {} accounts from storage", accounts.len());
                for (address, account) in accounts {
                    state_db.accounts.load_account(address, account);
                }
            }
            Err(e) => tracing::warn!("Could not load accounts: {}", e),
        }

        // 4. Initialize genesis if no blocks exist
        // CRITICAL: Must produce identical genesis hash as the standalone node.
        // The node calls initialize_genesis_state_with_profile() which:
        //   1. Creates canonical genesis block
        //   2. Initializes shared genesis state (accounts + model)
        //   3. Sets genesis.state_root = state_root from step 2
        //   4. Recalculates genesis.header.block_hash with the state root included
        // We must do exactly the same steps in the same order.
        let latest_height = storage.blocks.get_latest_height().unwrap_or(0);
        if latest_height == 0 {
            tracing::info!("No blocks found — initializing genesis (must match standalone node)");

            // Step 1: Create canonical genesis block
            let mut genesis = citrate_economics::genesis::create_canonical_genesis_block(
                citrate_economics::genesis::CANONICAL_GENESIS_TIMESTAMP,
            );

            // Step 2: Initialize shared genesis state (accounts + model)
            let executor = Arc::new(citrate_execution::executor::Executor::new(
                state_db.clone(),
            ));
            let genesis_config = if chain_id == 40204 {
                citrate_economics::genesis::GenesisConfig::testnet_beta()
            } else {
                citrate_economics::genesis::GenesisConfig::default()
            };
            let state_root_bytes = citrate_economics::genesis::initialize_shared_genesis_state(
                &executor,
                &genesis_config,
            );

            // Step 3: Set state root on the genesis block (this is what the node does)
            genesis.state_root = citrate_consensus::types::Hash::new(state_root_bytes);

            // Step 4: Recalculate block hash with state root included
            genesis.header.block_hash = citrate_economics::genesis::calculate_canonical_block_hash(&genesis);

            // Store genesis block
            if let Err(e) = storage.blocks.put_block(&genesis) {
                tracing::warn!("Genesis block storage: {}", e);
            }
            tracing::info!(
                "Genesis block stored: {} (state_root: {})",
                hex::encode(&genesis.header.block_hash.as_bytes()[..8]),
                hex::encode(&state_root_bytes[..8])
            );
        }

        // 5. Compute head info for handshake
        let head_height = storage.blocks.get_latest_height().unwrap_or(0);
        let head_hash = if head_height > 0 {
            storage.blocks.get_block_by_height(head_height).ok().flatten()
                .unwrap_or_default()
        } else {
            citrate_consensus::types::Hash::default()
        };
        let genesis_hash = storage.blocks.get_block_by_height(0).ok().flatten()
            .unwrap_or_default();
        let network_id = chain_id as u32;

        // 6. Create PeerManager
        let peer_manager = Arc::new(PeerManager::new(PeerManagerConfig {
            max_peers: 25,
            max_inbound: 12,
            max_outbound: 13,
            peer_timeout: Duration::from_secs(30),
            ban_duration: Duration::from_secs(3600),
            score_threshold: -100,
        }));

        // 7. Load or generate persistent Noise identity
        let noise_key_path = std::path::Path::new(data_dir).join("noise.key");
        let noise_keypair = if noise_key_path.exists() {
            let key_bytes = std::fs::read(&noise_key_path)
                .map_err(|e| AppError::Node(format!("Failed to read noise key: {}", e)))?;
            NoiseKeypair::from_bytes(&key_bytes)
                .map_err(|e| AppError::Node(format!("Failed to parse noise key: {}", e)))?
        } else {
            let kp = NoiseKeypair::generate();
            std::fs::write(&noise_key_path, kp.to_bytes())
                .map_err(|e| AppError::Node(format!("Failed to write noise key: {}", e)))?;
            tracing::info!("Generated new persistent Noise identity");
            kp
        };
        let local_peer_id = noise_keypair.derive_peer_id();
        tracing::info!(
            "Noise identity: {}... (peer_id={})",
            &noise_keypair.public_key_hex()[..16],
            local_peer_id
        );

        // 8. Create NetworkTransport with Noise encryption
        let transport = NetworkTransport::new(
            peer_manager.clone(),
            local_peer_id,
            HandshakeParams {
                network_id,
                genesis_hash,
                head_height,
                head_hash,
            },
        ).with_noise(noise_keypair);

        // 9. Start TCP listener — try configured port, fall back to OS-assigned
        let listen_result = {
            let primary: std::net::SocketAddr = "0.0.0.0:30304".parse()
                .map_err(|e| AppError::Node(format!("Invalid listen address: {}", e)))?;
            match transport.start_listener(primary).await {
                Ok(()) => {
                    tracing::info!("P2P listener started on {}", primary);
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!("Port 30304 busy ({}), trying OS-assigned port", e);
                    let fallback: std::net::SocketAddr = "0.0.0.0:0".parse()
                        .map_err(|e| AppError::Node(format!("Invalid listen address: {}", e)))?;
                    transport.start_listener(fallback).await
                        .map_err(|e| AppError::Node(format!("P2P listener failed on fallback: {}", e)))
                }
            }
        };
        listen_result?;

        // 10. Connect to bootstrap nodes from config (F-05 fix: not hardcoded)
        let configured_bootnodes = self.bootnodes.read().await.clone();
        let mut connected_count = 0u32;
        for s in &configured_bootnodes {
            // Resolve [identity@]host:port, performing DNS for hostnames so the
            // baked hostname-based testnet config (boot1.citrate.ai, …) connects
            // out of the box. Inlined rather than calling
            // citrate_network::resolve_bootnode because this repo pins an older
            // citrate-network rev that predates that shared helper; bump the pin
            // to de-duplicate once the chain rev is advanced federation-wide.
            let host_port = s.split_once('@').map(|(_, rest)| rest).unwrap_or(s.as_str()).trim();
            let addr = match host_port.parse::<std::net::SocketAddr>() {
                Ok(a) => a,
                Err(_) => match tokio::net::lookup_host(host_port)
                    .await
                    .ok()
                    .and_then(|mut it| it.next())
                {
                    Some(a) => a,
                    None => {
                        tracing::warn!("Cannot resolve bootnode address: {}", s);
                        continue;
                    }
                },
            };

            tracing::info!("=== Connecting to bootnode {} ===", addr);
            match transport.connect_to(addr).await {
                Ok(()) => {
                    connected_count += 1;
                    tracing::info!("=== CONNECTED to bootnode {} (Noise encrypted) ===", addr);
                }
                Err(e) => {
                    tracing::error!("=== BOOTNODE CONNECTION FAILED: {} ===", e);
                    tracing::error!("  Bootnode may be offline or running a different chain");
                }
            }
        }
        tracing::info!("Bootnode connection phase complete: {} connected", connected_count);

        // Request blocks from connected peers to start syncing
        if connected_count > 0 {
            // Ask peers for blocks starting from our head (genesis)
            let get_blocks = citrate_network::NetworkMessage::GetBlocks {
                from: genesis_hash,
                count: 100,
                step: 1,
            };
            let _ = peer_manager.broadcast(&get_blocks).await;
            tracing::info!("Requested blocks from peers (starting from genesis)");
        }

        // 11. Set up incoming message channel and handler
        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<(
            citrate_network::PeerId,
            citrate_network::NetworkMessage,
        )>(512);
        peer_manager.set_incoming(in_tx).await;

        // Spawn message handler — processes incoming blocks and stores them
        let storage_for_handler = storage.clone();
        let shutdown_for_handler = self.shutdown.clone();
        let notify_for_handler = self.shutdown_notify.clone();
        let msg_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg_opt = in_rx.recv() => {
                        match msg_opt {
                            Some((peer_id, msg)) => {
                                if shutdown_for_handler.load(std::sync::atomic::Ordering::SeqCst) {
                                    break;
                                }
                                match msg {
                                    citrate_network::NetworkMessage::NewBlock { block } => {
                                        let height = block.header.height;
                                        let hash_short = hex::encode(&block.header.block_hash.as_bytes()[..8]);
                                        if let Err(e) = storage_for_handler.blocks.put_block(&block) {
                                            tracing::warn!("Failed to store block {}: {}", height, e);
                                        } else {
                                            tracing::info!("Synced block #{} ({}) from {}", height, hash_short, peer_id);
                                        }
                                    }
                                    citrate_network::NetworkMessage::Blocks { blocks } => {
                                        tracing::info!("Received {} blocks from {}", blocks.len(), peer_id);
                                        for block in blocks {
                                            let height = block.header.height;
                                            if let Err(e) = storage_for_handler.blocks.put_block(&block) {
                                                tracing::debug!("Block {} store: {}", height, e);
                                            } else {
                                                tracing::info!("Synced block #{}", height);
                                            }
                                        }
                                    }
                                    citrate_network::NetworkMessage::HelloAck { head_height, .. } => {
                                        tracing::info!("Peer {} reports head height: {}", peer_id, head_height);
                                    }
                                    _ => {
                                        tracing::debug!("Message from {}: {:?}", peer_id, std::mem::discriminant(&msg));
                                    }
                                }
                            }
                            None => break, // Channel closed
                        }
                    }
                    _ = notify_for_handler.notified() => {
                        tracing::info!("Message handler: shutdown notification received");
                        break;
                    }
                }
            }
            tracing::info!("Message handler stopped");
        });

        // 12. Start discovery loop
        let discovery = Arc::new(Discovery::new(
            DiscoveryConfig {
                bootstrap_nodes: configured_bootnodes,
                max_peers: 25,
                ..Default::default()
            },
            peer_manager.clone(),
        ));
        discovery.init().await.ok();

        let discovery_loop = discovery.clone();
        let transport_loop = transport;
        let pm_loop = peer_manager.clone();
        let shutdown_flag = self.shutdown.clone();
        let notify_for_discovery = self.shutdown_notify.clone();
        let disc_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                // Use select! so shutdown_notify wakes us immediately instead of
                // waiting up to 10s for the next interval tick.
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = notify_for_discovery.notified() => {
                        tracing::info!("Discovery loop: shutdown notification received");
                        break;
                    }
                }
                if shutdown_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::info!("Discovery loop: shutdown flag set");
                    break;
                }
                let candidates = discovery_loop.find_peers().await;
                for (id, addr) in candidates {
                    match transport_loop.connect_to(addr).await {
                        Ok(_) => {
                            discovery_loop.mark_connected(&id).await;
                            discovery_loop.update_attempts(&id, true).await;
                        }
                        Err(_) => {
                            discovery_loop.update_attempts(&id, false).await;
                        }
                    }
                }
                let _ = pm_loop.broadcast(&citrate_network::NetworkMessage::GetPeers).await;
            }
            tracing::info!("Discovery loop stopped");
        });

        // Store JoinHandles so stop_node can await them for clean shutdown
        {
            let mut tasks = self.background_tasks.lock().await;
            tasks.push(msg_handle);
            tasks.push(disc_handle);
        }

        // Store references for status queries
        *self.storage.write().await = Some(storage);
        *self.peer_manager.write().await = Some(peer_manager);
        *self.state_db.write().await = Some(state_db);
        self.running.store(true, std::sync::atomic::Ordering::SeqCst);

        // 13. Auto-start IPFS daemon for model/artifact storage
        tracing::info!("Starting IPFS daemon...");
        let ipfs_config = citrate_storage::ipfs::DaemonConfig {
            auto_start: true,
            auto_download: true,
            ..Default::default()
        };
        let ipfs = Arc::new(citrate_storage::ipfs::IpfsDaemon::new(ipfs_config));
        match ipfs.initialize().await {
            Ok(()) => {
                tracing::info!("IPFS daemon initialized — models and artifacts can be stored");
                *self.ipfs_daemon.write().await = Some(ipfs);
            }
            Err(e) => {
                // IPFS failure is non-blocking — node works without it, just can't store models
                tracing::warn!("IPFS daemon failed to start: {} — model storage unavailable", e);
            }
        }

        tracing::info!("Embedded node started — connected to network");
        Ok(())
    }

    async fn stop_node(&self) -> Result<(), AppError> {
        tracing::info!("Stopping embedded node");
        // Lifecycle mutex — queues behind any in-progress start/stop
        let _lifecycle_guard = self.lifecycle.lock().await;

        // Signal background tasks to stop via flag AND instant notification
        self.shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        self.running.store(false, std::sync::atomic::Ordering::SeqCst);
        // Wake all background tasks immediately (no more waiting for 10s interval)
        self.shutdown_notify.notify_waiters();

        // Stop IPFS daemon first (no storage dependency)
        if let Some(ref ipfs) = *self.ipfs_daemon.read().await {
            if let Err(e) = ipfs.stop().await {
                tracing::warn!("IPFS daemon stop: {}", e);
            }
        }
        *self.ipfs_daemon.write().await = None;

        // Drop peer manager — closes the mpsc channel, which also unblocks the
        // message handler if it's waiting on in_rx.recv().
        *self.peer_manager.write().await = None;
        tracing::debug!("Peer manager dropped, awaiting background task JoinHandles");

        // Await all background task JoinHandles with a timeout.
        // This guarantees tasks have exited and dropped their Arc<StorageManager> clones.
        {
            let mut tasks = self.background_tasks.lock().await;
            for handle in tasks.drain(..) {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    handle,
                ).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("Background task panicked: {}", e),
                    Err(_) => {
                        tracing::warn!("Background task did not exit within 5s timeout");
                    }
                }
            }
        }

        // Verify Arc refcount — should be 1 (only self.storage holds it)
        if let Some(ref storage) = *self.storage.read().await {
            let count = Arc::strong_count(storage);
            if count > 1 {
                tracing::warn!(
                    "Storage Arc has {} strong refs after task shutdown (expected 1) — forcing 1s wait",
                    count
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }

        // Drop storage (releases RocksDB lock)
        *self.storage.write().await = None;
        *self.state_db.write().await = None;

        // Brief pause for OS to release the lock file descriptor
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Reset shutdown flag so start_node works again
        self.shutdown.store(false, std::sync::atomic::Ordering::SeqCst);

        tracing::info!("Embedded node stopped (storage released, ready for restart)");
        Ok(())
    }

    async fn get_block_height(&self) -> u64 {
        if let Some(ref storage) = *self.storage.read().await {
            storage.blocks.get_latest_height().unwrap_or(0)
        } else {
            0
        }
    }

    async fn get_peer_count(&self) -> u32 {
        if let Some(ref pm) = *self.peer_manager.read().await {
            let (total, _inbound, _outbound) = pm.get_peer_counts().await;
            total as u32
        } else {
            0
        }
    }

    async fn get_mempool_size(&self) -> usize {
        0 // Mempool wiring deferred — requires sequencer integration
    }

    async fn get_balance(&self, address: &[u8; 20]) -> String {
        // First try local state DB (works if we executed the blocks)
        if let Some(ref state_db) = *self.state_db.read().await {
            let addr = citrate_execution::types::Address(*address);
            let account = state_db.accounts.get_account(&addr);
            if !account.balance.is_zero() {
                return format!("{}", account.balance);
            }
        }
        // Fallback: query remote RPC for balance — only on testnet.
        // On devnet (no bootnodes), there's no remote RPC to query.
        // Data source: eth_getBalance via https://rpc.citrate.ai (testnet only)
        let bootnodes = self.bootnodes.read().await;
        if bootnodes.is_empty() {
            // Devnet — no remote RPC, return local-only balance
            return "0".to_string();
        }
        drop(bootnodes);

        let hex_addr = format!("0x{}", hex::encode(address));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBalance",
            "params": [hex_addr, "latest"],
            "id": 1,
        });
        let client = reqwest::Client::new();
        match client.post("https://rpc.citrate.ai")
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send().await
        {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
                        let hex = result.trim_start_matches("0x");
                        if let Ok(wei) = u128::from_str_radix(hex, 16) {
                            return format!("{}", wei);
                        }
                    }
                }
                tracing::warn!("Balance RPC returned unexpected format for {}", hex_addr);
                "0".to_string()
            }
            Err(e) => {
                tracing::warn!("Balance RPC failed for {}: {}", hex_addr, e);
                "0".to_string()
            }
        }
    }

    async fn get_block_summaries(&self, count: usize) -> Vec<BlockSummary> {
        let storage_guard = self.storage.read().await;
        let storage = match storage_guard.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let height = storage.blocks.get_latest_height().unwrap_or(0);
        if height == 0 {
            return Vec::new();
        }
        let start = if height > count as u64 { height - count as u64 + 1 } else { 1 };
        let mut blocks = Vec::new();
        for h in (start..=height).rev() {
            // Data source: RocksDB via citrate_storage::BlockStore
            // get_block_by_height → Hash, then get_block → Block
            let hash_opt = storage.blocks.get_block_by_height(h).ok().flatten();
            if let Some(hash) = hash_opt {
                let hex_hash = format!("0x{}", hash.to_hex());
                if let Ok(Some(block)) = storage.blocks.get_block(&hash) {
                    let proposer_hex = hex::encode(block.header.proposer_pubkey.as_bytes());
                    blocks.push(BlockSummary {
                        hash: hex_hash,
                        height: h,
                        timestamp: block.header.timestamp,
                        tx_count: block.transactions.len(),
                        selected_parent: format!("0x{}", block.header.selected_parent_hash.to_hex()),
                        blue_score: block.header.blue_score,
                        proposer: proposer_hex,
                    });
                } else {
                    // Header exists but full block missing — show what we have
                    blocks.push(BlockSummary {
                        hash: hex_hash,
                        height: h,
                        timestamp: 0,
                        tx_count: 0,
                        selected_parent: String::new(),
                        blue_score: 0,
                        proposer: String::new(),
                    });
                }
            }
        }
        blocks
    }

    async fn set_bootnodes(&self, bootnodes: Vec<String>) {
        *self.bootnodes.write().await = bootnodes;
    }

    /// Full per-transaction detail for a block, including receipt status + gas used.
    ///
    /// Data source: `citrate_storage::BlockStore::get_block` (CF_BLOCKS) for the tx
    /// list; `citrate_storage::TransactionStore::get_receipt` (CF_RECEIPTS) for
    /// `status` + `gas_used` + `effective_gas_price`. Returns empty when the
    /// block hash is malformed or absent.
    async fn get_block_transactions(&self, block_hash: &str) -> Vec<BlockTxDetail> {
        let storage_guard = self.storage.read().await;
        let storage = match storage_guard.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };

        let hash_hex = block_hash.trim_start_matches("0x");
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => return Vec::new(),
        };
        let mut h_arr = [0u8; 32];
        h_arr.copy_from_slice(&hash_bytes);
        let block_hash_typed = citrate_consensus::types::Hash::new(h_arr);

        let block = match storage.blocks.get_block(&block_hash_typed) {
            Ok(Some(b)) => b,
            _ => return Vec::new(),
        };

        let derive_addr = |pk: &citrate_consensus::types::PublicKey| -> String {
            let bytes = pk.as_bytes();
            let is_evm = bytes[20..].iter().all(|&b| b == 0)
                && !bytes[..20].iter().all(|&b| b == 0);
            if is_evm {
                format!("0x{}", hex::encode(&bytes[..20]))
            } else {
                use sha3::{Digest, Keccak256};
                let kh = Keccak256::digest(bytes);
                format!("0x{}", hex::encode(&kh[12..]))
            }
        };

        let mut out = Vec::with_capacity(block.transactions.len());
        for tx in &block.transactions {
            let receipt = storage
                .transactions
                .get_receipt(&tx.hash)
                .ok()
                .flatten();
            let (status, gas_used, eff_gas) = match &receipt {
                Some(r) => (
                    if r.status {
                        TxStatus::Confirmed
                    } else {
                        TxStatus::Failed
                    },
                    r.gas_used,
                    r.effective_gas_price,
                ),
                None => (TxStatus::ReceiptMissing, 0, 0),
            };

            out.push(BlockTxDetail {
                tx_hash: format!("0x{}", tx.hash.to_hex()),
                from: derive_addr(&tx.from),
                to: tx.to.as_ref().map(derive_addr),
                value_wei: tx.value.to_string(),
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                gas_price_wei: tx.gas_price,
                gas_used,
                effective_gas_price_wei: eff_gas,
                status,
                input_hex: hex::encode(&tx.data),
                block_height: block.header.height,
                eth_tx_type: tx.eth_tx_type,
            });
        }
        out
    }

    /// Scan local blocks for transactions involving the given address.
    /// Data source: RocksDB block store — iterates recent blocks and filters txs.
    async fn get_transactions_for(&self, address: &str, limit: usize) -> Vec<TxSummary> {
        let storage_guard = self.storage.read().await;
        let storage = match storage_guard.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let height = storage.blocks.get_latest_height().unwrap_or(0);
        if height == 0 {
            return Vec::new();
        }

        let addr_lower = address.to_lowercase();
        let scan_depth = 500.min(height);
        let start = height.saturating_sub(scan_depth) + 1;
        let mut txs = Vec::new();

        for h in (start..=height).rev() {
            if txs.len() >= limit {
                break;
            }
            let hash_opt = storage.blocks.get_block_by_height(h).ok().flatten();
            if let Some(hash) = hash_opt {
                if let Ok(Some(block)) = storage.blocks.get_block(&hash) {
                    for tx in &block.transactions {
                        // Derive EVM address from public key using the same logic
                        // as Address::from_public_key — embedded EVM (first 20 bytes
                        // if last 12 are zero) or Keccak256 hash of full key.
                        let derive_addr = |pk: &citrate_consensus::types::PublicKey| -> String {
                            let bytes = pk.as_bytes();
                            let is_evm = bytes[20..].iter().all(|&b| b == 0)
                                && !bytes[..20].iter().all(|&b| b == 0);
                            if is_evm {
                                format!("0x{}", hex::encode(&bytes[..20]))
                            } else {
                                use sha3::{Digest, Keccak256};
                                let hash = Keccak256::digest(bytes);
                                format!("0x{}", hex::encode(&hash[12..]))
                            }
                        };
                        let from = derive_addr(&tx.from);
                        let to = tx.to.as_ref()
                            .map(derive_addr)
                            .unwrap_or_else(|| "contract creation".to_string());
                        let from_match = from.to_lowercase() == addr_lower;
                        let to_match = to.to_lowercase() == addr_lower;
                        if from_match || to_match {
                            txs.push(TxSummary {
                                hash: format!("0x{}", tx.hash.to_hex()),
                                tx_type: if from_match { "send".to_string() } else { "receive".to_string() },
                                amount: citrate_wallet_core::format::wei_to_salt(tx.value),
                                counterparty: if from_match { to.clone() } else { from.clone() },
                                status: "confirmed".to_string(),
                                timestamp: block.header.timestamp.to_string(),
                            });
                        }
                    }
                }
            }
        }
        txs.truncate(limit);
        txs
    }
}

/// Test-only backend that returns defaults. Not compiled in release builds.
#[cfg(test)]
pub struct TestNodeBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl NodeBackend for TestNodeBackend {
    async fn start_node(&self, _chain_id: u64, _data_dir: &str) -> Result<(), AppError> { Ok(()) }
    async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
    async fn get_block_height(&self) -> u64 { 0 }
    async fn get_peer_count(&self) -> u32 { 0 }
    async fn get_mempool_size(&self) -> usize { 0 }
    async fn get_balance(&self, _address: &[u8; 20]) -> String { "0".to_string() }
}

/// Node lifecycle and chain queries.
pub struct NodeService {
    pub config: Arc<RwLock<AppConfig>>,
    events: Arc<EventBus>,
    status: Arc<RwLock<NodeStatus>>,
    backend: Arc<dyn NodeBackend>,
}

impl NodeService {
    /// Create with the embedded node backend (production).
    pub fn new(config: Arc<RwLock<AppConfig>>, events: Arc<EventBus>) -> Self {
        Self {
            config,
            events,
            status: Arc::new(RwLock::new(NodeStatus::default())),
            backend: Arc::new(EmbeddedNodeBackend::new()),
        }
    }

    /// Create with an injected backend (for testing or alternative node).
    pub fn with_backend(
        config: Arc<RwLock<AppConfig>>,
        events: Arc<EventBus>,
        backend: Arc<dyn NodeBackend>,
    ) -> Self {
        Self {
            config,
            events,
            status: Arc::new(RwLock::new(NodeStatus::default())),
            backend,
        }
    }

    /// Get current node status
    pub async fn get_status(&self) -> NodeStatus {
        self.status.read().await.clone()
    }

    /// Start the embedded node
    pub async fn start(&self) -> Result<(), AppError> {
        let config = self.config.read().await;
        tracing::info!("Starting node for network={}, chain_id={}", config.network, config.chain_id);

        self.backend.start_node(config.chain_id, &config.data_dir).await?;

        let mut status = self.status.write().await;
        status.running = true;
        status.chain_id = config.chain_id;

        self.events.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: status.block_height,
            peer_count: status.peer_count,
            syncing: false,
        });

        Ok(())
    }

    /// Stop the embedded node
    pub async fn stop(&self) -> Result<(), AppError> {
        tracing::info!("Stopping node");
        self.backend.stop_node().await?;

        let mut status = self.status.write().await;
        status.running = false;

        self.events.publish(AppEvent::NodeStatusChanged {
            running: false,
            block_height: status.block_height,
            peer_count: 0,
            syncing: false,
        });

        Ok(())
    }

    /// Refresh status from backend
    pub async fn refresh_status(&self) {
        if !self.status.read().await.running {
            return;
        }
        let height = self.backend.get_block_height().await;
        let peers = self.backend.get_peer_count().await;
        let mempool = self.backend.get_mempool_size().await;

        let mut status = self.status.write().await;
        let changed = status.block_height != height || status.peer_count != peers;
        status.block_height = height;
        status.peer_count = peers;
        status.mempool_size = mempool;

        if changed {
            self.events.publish(AppEvent::NodeStatusChanged {
                running: true,
                block_height: height,
                peer_count: peers,
                syncing: false,
            });
        }
    }

    /// Get balance for an address (in wei)
    pub async fn get_balance(&self, address: &str) -> Result<String, AppError> {
        let addr = address.strip_prefix("0x").unwrap_or(address);
        if addr.len() != 40 {
            return Err(AppError::InvalidAddress(address.to_string()));
        }
        let addr_bytes = hex::decode(addr)
            .map_err(|_| AppError::InvalidAddress(address.to_string()))?;
        let mut addr_20 = [0u8; 20];
        addr_20.copy_from_slice(&addr_bytes);
        Ok(self.backend.get_balance(&addr_20).await)
    }

    /// Get recent blocks from local storage (reads real data from RocksDB)
    pub async fn get_recent_blocks(&self, count: usize) -> Result<Vec<BlockSummary>, AppError> {
        Ok(self.backend.get_block_summaries(count).await)
    }

    /// Full transaction details for every tx in a block.
    /// Data source: see `NodeBackend::get_block_transactions`.
    pub async fn get_block_transactions(&self, block_hash: &str) -> Vec<BlockTxDetail> {
        self.backend.get_block_transactions(block_hash).await
    }

    /// Update bootnodes for network switching
    pub async fn update_bootnodes(&self, bootnodes: Vec<String>) {
        self.backend.set_bootnodes(bootnodes).await;
    }

    /// Get the reward address (from wallet config — first account address)
    pub async fn get_reward_address(&self) -> Option<String> {
        // Reward address is the primary wallet address (first account)
        // This is used for block production coinbase on devnet
        None // Deferred: needs WalletService reference, not available in NodeService
    }

    /// Get recent transactions for an address via the backend.
    pub async fn get_transactions_for_address(&self, address: &str, limit: usize) -> Vec<TxSummary> {
        self.backend.get_transactions_for(address, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> NodeService {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        NodeService::with_backend(config, events, Arc::new(TestNodeBackend))
    }

    #[tokio::test]
    async fn test_initial_status_is_stopped() {
        let svc = test_service();
        let status = svc.get_status().await;
        assert!(!status.running);
        assert_eq!(status.chain_id, 40204);
    }

    #[tokio::test]
    async fn test_start_updates_status() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        let status = svc.get_status().await;
        assert!(status.running);
    }

    #[tokio::test]
    async fn test_stop_updates_status() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        svc.stop().await.expect("stop succeeded");
        let status = svc.get_status().await;
        assert!(!status.running);
    }

    #[tokio::test]
    async fn test_start_publishes_event() {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let svc = NodeService::with_backend(config, events, Arc::new(TestNodeBackend));

        svc.start().await.expect("start succeeded");

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::NodeStatusChanged { running, .. } => assert!(running),
            _ => panic!("Expected NodeStatusChanged"),
        }
    }

    #[tokio::test]
    async fn test_invalid_address_rejected() {
        let svc = test_service();
        let result = svc.get_balance("0xbad").await;
        assert!(result.is_err());
        match result.expect_err("expected error") {
            AppError::InvalidAddress(_) => {}
            other => panic!("Expected InvalidAddress, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_valid_address_accepted() {
        let svc = test_service();
        let result = svc.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_config_has_bootnode() {
        let svc = test_service();
        let config = svc.config.read().await;
        assert!(!config.bootnodes.is_empty(), "Default config must include testnet bootnode");
        // Default bootnodes are DNS hostnames (boot{1,2,3}.citrate.ai,
        // rpc.citrate.ai), resolved via citrate_network::resolve_bootnode —
        // the literal VPS IP was retired in the bootnode-DNS migration.
        assert!(
            config.bootnodes[0].contains("citrate.ai"),
            "Bootnode must point to citrate.ai infra"
        );
    }

    #[tokio::test]
    async fn test_status_chain_id_matches_config() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        let status = svc.get_status().await;
        let config = svc.config.read().await;
        assert_eq!(status.chain_id, config.chain_id);
    }

    #[tokio::test]
    async fn test_get_recent_blocks_returns_vec() {
        let svc = test_service();
        let blocks = svc.get_recent_blocks(5).await;
        assert!(blocks.is_ok());
    }

    #[tokio::test]
    async fn test_reward_address_none_before_wallet() {
        let svc = test_service();
        assert!(svc.get_reward_address().await.is_none());
    }

    #[tokio::test]
    async fn test_start_stop_cycle() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        assert!(svc.get_status().await.running);
        svc.stop().await.expect("stop succeeded");
        assert!(!svc.get_status().await.running);
        // Can restart
        svc.start().await.expect("start succeeded");
        assert!(svc.get_status().await.running);
    }

    #[tokio::test]
    async fn test_stop_publishes_event() {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let svc = NodeService::with_backend(config, events, Arc::new(TestNodeBackend));

        svc.start().await.expect("start succeeded");
        let _ = rx.recv().await.expect("event received"); // consume start event
        svc.stop().await.expect("stop succeeded");

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::NodeStatusChanged { running, peer_count, .. } => {
                assert!(!running);
                assert_eq!(peer_count, 0);
            }
            _ => panic!("Expected NodeStatusChanged on stop"),
        }
    }

    #[tokio::test]
    async fn test_refresh_does_nothing_when_stopped() {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let svc = NodeService::with_backend(config, events, Arc::new(TestNodeBackend));

        // Not started — refresh should be a no-op
        svc.refresh_status().await;

        // No event should be published
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.recv(),
        ).await;
        assert!(result.is_err(), "No event expected when node is stopped");
    }

    #[tokio::test]
    async fn test_default_status_fields() {
        let svc = test_service();
        let status = svc.get_status().await;
        assert_eq!(status.block_height, 0);
        assert_eq!(status.peer_count, 0);
        assert_eq!(status.mempool_size, 0);
        assert_eq!(status.dag_tips, 0);
        assert!(!status.syncing);
        assert_eq!(status.uptime_seconds, 0);
    }

    #[tokio::test]
    async fn test_address_with_0x_prefix() {
        let svc = test_service();
        let result = svc.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_address_without_0x_prefix() {
        let svc = test_service();
        let result = svc.get_balance("b5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_address_too_short() {
        let svc = test_service();
        let result = svc.get_balance("0x1234").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_address_too_long() {
        let svc = test_service();
        let result = svc.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129FF").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_address_invalid_hex() {
        let svc = test_service();
        let result = svc.get_balance("0xGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_address() {
        let svc = test_service();
        let result = svc.get_balance("").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_with_backend_constructor() {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(TestNodeBackend);
        let svc = NodeService::with_backend(config, events, backend);
        let status = svc.get_status().await;
        assert!(!status.running);
    }

    #[tokio::test]
    async fn test_custom_backend() {
        struct CustomBackend;

        #[async_trait::async_trait]
        impl NodeBackend for CustomBackend {
            async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> { Ok(()) }
            async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
            async fn get_block_height(&self) -> u64 { 42 }
            async fn get_peer_count(&self) -> u32 { 7 }
            async fn get_mempool_size(&self) -> usize { 3 }
            async fn get_balance(&self, _: &[u8; 20]) -> String { "1000000".to_string() }
        }

        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let svc = NodeService::with_backend(config, events, Arc::new(CustomBackend));
        svc.start().await.expect("start succeeded");

        let balance = svc.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await.expect("async operation succeeded");
        assert_eq!(balance, "1000000");
    }

    #[tokio::test]
    async fn test_refresh_publishes_on_change() {
        struct ChangingBackend {
            call_count: std::sync::atomic::AtomicU64,
        }

        #[async_trait::async_trait]
        impl NodeBackend for ChangingBackend {
            async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> { Ok(()) }
            async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
            async fn get_block_height(&self) -> u64 {
                // Returns 1, 2, 3, ... (starts at 1 so first refresh sees a change from 0)
                self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            }
            async fn get_peer_count(&self) -> u32 { 1 }
            async fn get_mempool_size(&self) -> usize { 0 }
            async fn get_balance(&self, _: &[u8; 20]) -> String { "0".to_string() }
        }

        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let backend = Arc::new(ChangingBackend {
            call_count: std::sync::atomic::AtomicU64::new(0),
        });
        let svc = NodeService::with_backend(config, events, backend);
        svc.start().await.expect("start succeeded");
        let _ = rx.recv().await.expect("event received"); // consume start event

        svc.refresh_status().await; // height changes from 0 to 1

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::NodeStatusChanged { block_height, .. } => {
                assert_eq!(block_height, 1, "Height should be 1 after first refresh");
            }
            _ => panic!("Expected NodeStatusChanged"),
        }
    }

    #[tokio::test]
    async fn test_double_start_ok() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        svc.start().await.expect("start succeeded");
        assert!(svc.get_status().await.running);
    }

    #[tokio::test]
    async fn test_double_stop_ok() {
        let svc = test_service();
        svc.start().await.expect("start succeeded");
        svc.stop().await.expect("stop succeeded");
        svc.stop().await.expect("stop succeeded");
        assert!(!svc.get_status().await.running);
    }

    #[tokio::test]
    async fn test_stop_without_start_ok() {
        let svc = test_service();
        svc.stop().await.expect("stop succeeded");
        assert!(!svc.get_status().await.running);
    }

    #[tokio::test]
    async fn test_node_status_clone() {
        let status = NodeStatus::default();
        let cloned = status.clone();
        assert_eq!(cloned.chain_id, status.chain_id);
        assert_eq!(cloned.running, status.running);
    }

    #[tokio::test]
    async fn test_node_status_debug() {
        let status = NodeStatus::default();
        let debug = format!("{:?}", status);
        assert!(debug.contains("running"));
        assert!(debug.contains("chain_id"));
    }

    #[tokio::test]
    async fn test_block_summary_fields() {
        let block = BlockSummary {
            hash: "0xabc".to_string(),
            height: 100,
            timestamp: 1234567890,
            tx_count: 5,
            selected_parent: "0xdef".to_string(),
            blue_score: 42,
            proposer: "0xproposer".to_string(),
        };
        assert_eq!(block.height, 100);
        assert_eq!(block.tx_count, 5);
        assert_eq!(block.blue_score, 42);
    }

    #[tokio::test]
    async fn test_config_default_bootnode_format() {
        let svc = test_service();
        let config = svc.config.read().await;
        let bootnode = &config.bootnodes[0];
        // Format: noise_<hex-pubkey>@<host>:<port> — see lib.rs default bootnodes.
        assert!(
            bootnode.starts_with("noise_"),
            "Bootnode should carry a noise identity prefix"
        );
        assert!(bootnode.contains("citrate.ai"), "Bootnode should point to citrate.ai infra");
        assert!(bootnode.contains(":30303"), "Bootnode should specify port");
    }

    #[tokio::test]
    async fn test_balance_returns_stub_zero() {
        let svc = test_service();
        let balance = svc.get_balance("0xb5ddd4eb356ddf3bf51eb3aec1ed28213be59129").await.expect("async operation succeeded");
        assert_eq!(balance, "0");
    }

    #[tokio::test]
    async fn test_failing_backend() {
        struct FailingBackend;

        #[async_trait::async_trait]
        impl NodeBackend for FailingBackend {
            async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> {
                Err(AppError::Node("failed to start".into()))
            }
            async fn stop_node(&self) -> Result<(), AppError> {
                Err(AppError::Node("failed to stop".into()))
            }
            async fn get_block_height(&self) -> u64 { 0 }
            async fn get_peer_count(&self) -> u32 { 0 }
            async fn get_mempool_size(&self) -> usize { 0 }
            async fn get_balance(&self, _: &[u8; 20]) -> String { "0".into() }
        }

        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(EventBus::new());
        let svc = NodeService::with_backend(config, events, Arc::new(FailingBackend));
        let result = svc.start().await;
        assert!(result.is_err());
        assert!(!svc.get_status().await.running);
    }

    // ---- BFR-INT-5b: BlockTxDetail ----

    #[test]
    fn tx_status_as_str_round_trip() {
        assert_eq!(TxStatus::Confirmed.as_str(), "confirmed");
        assert_eq!(TxStatus::Failed.as_str(), "failed");
        assert_eq!(TxStatus::ReceiptMissing.as_str(), "receipt-missing");
    }

    #[tokio::test]
    async fn test_get_block_transactions_default_returns_empty() {
        // TestNodeBackend uses the trait's default impl, which returns an empty vec.
        let svc = test_service();
        let txs = svc.get_block_transactions("0xdeadbeef").await;
        assert!(txs.is_empty(), "default backend should return empty");
    }

    #[tokio::test]
    async fn embedded_get_block_transactions_rejects_malformed_hash() {
        // Embedded backend with no storage initialized — should bail before storage anyway,
        // but the malformed-hash branch is the contract we want to exercise.
        let backend = EmbeddedNodeBackend::new();
        // Not a hex string at all.
        assert!(backend.get_block_transactions("not-a-hex").await.is_empty());
        // Hex but wrong length.
        assert!(backend.get_block_transactions("0xabcd").await.is_empty());
        // Hex of correct length but no storage initialised — still empty (no panic).
        let bogus = "0x".to_string() + &"ab".repeat(32);
        assert!(backend.get_block_transactions(&bogus).await.is_empty());
    }
}
