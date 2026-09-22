//! End-to-end node lifecycle tests using AppCore services.
//!
//! These tests exercise the node service lifecycle, status updates,
//! balance queries, block retrieval, and event bus integration through
//! the headless service layer.
//!
//! No Slint UI is instantiated — we test the service orchestration that the
//! UI shell would call.

use std::sync::Arc;
use tokio::sync::RwLock;

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::services::node_service::{
    BlockSummary, NodeBackend, NodeService, NodeStatus,
};
use citrate_desktop_app::AppConfig;

// ============================================================================
// Configurable test node backend — tracks state for realistic lifecycle tests
// ============================================================================

struct E2eNodeBackend {
    running: std::sync::atomic::AtomicBool,
    block_height: std::sync::atomic::AtomicU64,
    peer_count: std::sync::atomic::AtomicU32,
    mempool_size: std::sync::atomic::AtomicUsize,
    /// Pre-loaded block summaries for get_block_summaries
    blocks: tokio::sync::RwLock<Vec<BlockSummary>>,
}

impl E2eNodeBackend {
    fn new() -> Self {
        Self {
            running: std::sync::atomic::AtomicBool::new(false),
            block_height: std::sync::atomic::AtomicU64::new(0),
            peer_count: std::sync::atomic::AtomicU32::new(0),
            mempool_size: std::sync::atomic::AtomicUsize::new(0),
            blocks: tokio::sync::RwLock::new(Vec::new()),
        }
    }

    fn with_blocks(blocks: Vec<BlockSummary>) -> Self {
        Self {
            running: std::sync::atomic::AtomicBool::new(false),
            block_height: std::sync::atomic::AtomicU64::new(0),
            peer_count: std::sync::atomic::AtomicU32::new(0),
            mempool_size: std::sync::atomic::AtomicUsize::new(0),
            blocks: tokio::sync::RwLock::new(blocks),
        }
    }

    fn set_block_height(&self, height: u64) {
        self.block_height
            .store(height, std::sync::atomic::Ordering::SeqCst);
    }

    fn set_peer_count(&self, count: u32) {
        self.peer_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    fn set_mempool_size(&self, size: usize) {
        self.mempool_size
            .store(size, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl NodeBackend for E2eNodeBackend {
    async fn start_node(&self, _chain_id: u64, _data_dir: &str) -> Result<(), AppError> {
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Simulate genesis block
        if self.block_height.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            self.block_height
                .store(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    async fn stop_node(&self) -> Result<(), AppError> {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn get_block_height(&self) -> u64 {
        self.block_height.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn get_peer_count(&self) -> u32 {
        self.peer_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn get_mempool_size(&self) -> usize {
        self.mempool_size.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn get_balance(&self, address: &[u8; 20]) -> String {
        // Return a deterministic balance based on the first byte
        let value: u128 = (address[0] as u128) * 1_000_000_000_000_000_000;
        format!("{}", value)
    }

    async fn get_block_summaries(&self, count: usize) -> Vec<BlockSummary> {
        let blocks = self.blocks.read().await;
        blocks.iter().take(count).cloned().collect()
    }
}

// ============================================================================
// Failing node backend — simulates node startup failure
// ============================================================================

struct FailingNodeBackend;

#[async_trait::async_trait]
impl NodeBackend for FailingNodeBackend {
    async fn start_node(&self, _chain_id: u64, _data_dir: &str) -> Result<(), AppError> {
        Err(AppError::Node(
            "RocksDB lock contention — another node instance is running".to_string(),
        ))
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
// Helper: build a NodeService with a test backend
// ============================================================================

fn make_node_service(backend: Arc<dyn NodeBackend>) -> (Arc<NodeService>, Arc<EventBus>) {
    let events = Arc::new(EventBus::new());
    let config = Arc::new(RwLock::new(AppConfig::default()));
    let node = Arc::new(NodeService::with_backend(config, events.clone(), backend));
    (node, events)
}

fn make_default_node_service() -> (Arc<NodeService>, Arc<EventBus>, Arc<E2eNodeBackend>) {
    let backend = Arc::new(E2eNodeBackend::new());
    let events = Arc::new(EventBus::new());
    let config = Arc::new(RwLock::new(AppConfig::default()));
    let node = Arc::new(NodeService::with_backend(
        config,
        events.clone(),
        backend.clone(),
    ));
    (node, events, backend)
}

// ============================================================================
// Tests: Start node — verify status changes to running
// ============================================================================

#[tokio::test]
async fn test_start_node_sets_running_status() {
    let (node, _events, _backend) = make_default_node_service();

    // Before start
    let status = node.get_status().await;
    assert!(!status.running, "Node should not be running before start");

    // Start
    node.start().await.expect("node start should succeed");

    // After start
    let status = node.get_status().await;
    assert!(status.running, "Node should be running after start");
    assert_eq!(
        status.chain_id, 40204,
        "Chain ID should match config default"
    );
}

// ============================================================================
// Tests: Start node — verify NodeStatusChanged event published
// ============================================================================

#[tokio::test]
async fn test_start_node_publishes_running_event() {
    let (node, events, _backend) = make_default_node_service();
    let mut rx = events.subscribe();

    node.start().await.expect("node start should succeed");

    let event = rx
        .recv()
        .await
        .expect("should receive NodeStatusChanged event on start");
    match event {
        AppEvent::NodeStatusChanged { running, .. } => {
            assert!(running, "Event should indicate running=true");
        }
        other => panic!("Expected NodeStatusChanged event, got {:?}", other),
    }
}

// ============================================================================
// Tests: Stop node — verify status changes to stopped
// ============================================================================

#[tokio::test]
async fn test_stop_node_sets_stopped_status() {
    let (node, _events, _backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");
    assert!(node.get_status().await.running);

    node.stop().await.expect("node stop should succeed");

    let status = node.get_status().await;
    assert!(!status.running, "Node should not be running after stop");
}

// ============================================================================
// Tests: Stop node — verify event published
// ============================================================================

#[tokio::test]
async fn test_stop_node_publishes_stopped_event() {
    let (node, events, _backend) = make_default_node_service();
    let mut rx = events.subscribe();

    node.start().await.expect("node start should succeed");

    // Consume the start event
    let _start_event = rx.recv().await.expect("should receive start event");

    node.stop().await.expect("node stop should succeed");

    let event = rx
        .recv()
        .await
        .expect("should receive NodeStatusChanged event on stop");
    match event {
        AppEvent::NodeStatusChanged { running, .. } => {
            assert!(!running, "Event should indicate running=false");
        }
        other => panic!("Expected NodeStatusChanged event on stop, got {:?}", other),
    }
}

// ============================================================================
// Tests: Start/stop cycle
// ============================================================================

#[tokio::test]
async fn test_start_stop_cycle_multiple_times() {
    let (node, _events, _backend) = make_default_node_service();

    for i in 0..3 {
        node.start()
            .await
            .unwrap_or_else(|_| panic!("start cycle {} should succeed", i));
        assert!(
            node.get_status().await.running,
            "Node should be running in cycle {}",
            i
        );

        node.stop()
            .await
            .unwrap_or_else(|_| panic!("stop cycle {} should succeed", i));
        assert!(
            !node.get_status().await.running,
            "Node should be stopped after cycle {}",
            i
        );
    }
}

// ============================================================================
// Tests: Refresh status updates block height
// ============================================================================

#[tokio::test]
async fn test_refresh_status_updates_block_height() {
    let (node, _events, backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");

    // Simulate blocks being produced
    backend.set_block_height(42);
    node.refresh_status().await;

    let status = node.get_status().await;
    assert_eq!(
        status.block_height, 42,
        "Block height should reflect backend value after refresh"
    );
}

#[tokio::test]
async fn test_refresh_status_updates_peer_count() {
    let (node, _events, backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");

    backend.set_peer_count(5);
    node.refresh_status().await;

    let status = node.get_status().await;
    assert_eq!(
        status.peer_count, 5,
        "Peer count should reflect backend value after refresh"
    );
}

#[tokio::test]
async fn test_refresh_status_publishes_event_on_change() {
    let (node, events, backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");

    // Subscribe after start to skip the start event
    let mut rx = events.subscribe();

    // Change height — refresh should publish event
    backend.set_block_height(10);
    node.refresh_status().await;

    let event = rx
        .recv()
        .await
        .expect("should receive event after status change");
    match event {
        AppEvent::NodeStatusChanged {
            block_height,
            running,
            ..
        } => {
            assert!(running);
            assert_eq!(block_height, 10);
        }
        other => panic!("Expected NodeStatusChanged, got {:?}", other),
    }
}

#[tokio::test]
async fn test_refresh_status_skipped_when_stopped() {
    let (node, events, backend) = make_default_node_service();

    // Node is stopped — refresh should be a no-op
    backend.set_block_height(999);
    let mut rx = events.subscribe();

    node.refresh_status().await;

    // No event should be published
    let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        result.is_err(),
        "No event should be published when node is stopped"
    );
}

// ============================================================================
// Tests: Get recent blocks — verify format
// ============================================================================

#[tokio::test]
async fn test_get_recent_blocks_format() {
    let blocks = vec![
        BlockSummary {
            hash: "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string(),
            height: 100,
            timestamp: 1711700000,
            tx_count: 5,
            selected_parent: "0x1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
            blue_score: 98,
            proposer: String::new(),
        },
        BlockSummary {
            hash: "0xfeedface0000000000000000000000000000000000000000000000000000dead".to_string(),
            height: 99,
            timestamp: 1711699998,
            tx_count: 0,
            selected_parent: "0x2222222222222222222222222222222222222222222222222222222222222222"
                .to_string(),
            blue_score: 97,
            proposer: String::new(),
        },
    ];

    let backend = Arc::new(E2eNodeBackend::with_blocks(blocks.clone()));
    let (node, _events) = make_node_service(backend);

    let recent = node
        .get_recent_blocks(10)
        .await
        .expect("get_recent_blocks should succeed");

    assert_eq!(recent.len(), 2, "Should return 2 blocks");

    // Verify first block format
    assert!(
        recent[0].hash.starts_with("0x"),
        "Block hash should start with 0x"
    );
    assert_eq!(recent[0].height, 100);
    assert!(recent[0].timestamp > 0, "Timestamp should be non-zero");
    assert_eq!(recent[0].tx_count, 5);
    assert!(
        !recent[0].selected_parent.is_empty(),
        "Selected parent should not be empty"
    );
    assert!(recent[0].blue_score > 0, "Blue score should be non-zero");

    // Verify second block
    assert_eq!(recent[1].height, 99);
    assert_eq!(
        recent[1].tx_count, 0,
        "Block with no transactions should have tx_count=0"
    );
}

#[tokio::test]
async fn test_get_recent_blocks_empty_when_no_blocks() {
    let (node, _events, _backend) = make_default_node_service();

    let recent = node
        .get_recent_blocks(10)
        .await
        .expect("get_recent_blocks should succeed even with no blocks");

    assert!(
        recent.is_empty(),
        "Should return empty list when no blocks available"
    );
}

#[tokio::test]
async fn test_get_recent_blocks_respects_count_limit() {
    let mut blocks = Vec::new();
    for i in 0..20 {
        blocks.push(BlockSummary {
            hash: format!("0x{:064x}", i),
            height: 100 - i as u64,
            timestamp: 1711700000 - i as u64,
            tx_count: i,
            selected_parent: format!("0x{:064x}", i + 1),
            blue_score: 100 - i as u64,
            proposer: String::new(),
        });
    }

    let backend = Arc::new(E2eNodeBackend::with_blocks(blocks));
    let (node, _events) = make_node_service(backend);

    let recent = node
        .get_recent_blocks(5)
        .await
        .expect("get_recent_blocks should succeed");

    assert_eq!(recent.len(), 5, "Should return at most the requested count");
}

// ============================================================================
// Tests: Node status fields accessible
// ============================================================================

#[tokio::test]
async fn test_node_status_all_fields_accessible() {
    let (node, _events, backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");

    backend.set_block_height(42);
    backend.set_peer_count(3);
    backend.set_mempool_size(10);
    node.refresh_status().await;

    let status = node.get_status().await;

    // All fields that the UI binds to must be accessible
    let _running: bool = status.running;
    let _height: u64 = status.block_height;
    let _peers: u32 = status.peer_count;
    let _mempool: usize = status.mempool_size;
    let _dag_tips: usize = status.dag_tips;
    let _syncing: bool = status.syncing;
    let _chain_id: u64 = status.chain_id;
    let _uptime: u64 = status.uptime_seconds;

    assert!(status.running);
    assert_eq!(status.block_height, 42);
    assert_eq!(status.peer_count, 3);
    assert_eq!(status.chain_id, 40204);
}

// ============================================================================
// Tests: NodeStatus default represents offline node
// ============================================================================

#[tokio::test]
async fn test_node_status_default_is_offline() {
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

// ============================================================================
// Tests: Balance query
// ============================================================================

#[tokio::test]
async fn test_get_balance_valid_address() {
    let (node, _events, _backend) = make_default_node_service();

    // Address with first byte = 0x01 → balance = 1 * 10^18
    let result = node
        .get_balance("0x0100000000000000000000000000000000000000")
        .await
        .expect("balance query should succeed for valid address");

    let wei: u128 = result.parse().expect("balance should be parseable as u128");
    assert_eq!(wei, 1_000_000_000_000_000_000u128);
}

#[tokio::test]
async fn test_get_balance_invalid_address_rejected() {
    let (node, _events, _backend) = make_default_node_service();

    // Too short
    let result = node.get_balance("0xbad").await;
    match result {
        Err(AppError::InvalidAddress(_)) => { /* expected */ }
        other => panic!("Expected InvalidAddress for short address, got {:?}", other),
    }

    // Invalid hex
    let result = node
        .get_balance("0xZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ")
        .await;
    match result {
        Err(AppError::InvalidAddress(_)) => { /* expected */ }
        other => panic!("Expected InvalidAddress for invalid hex, got {:?}", other),
    }
}

#[tokio::test]
async fn test_get_balance_without_0x_prefix() {
    let (node, _events, _backend) = make_default_node_service();

    // Without 0x prefix — should still work (strip_prefix handles this)
    let result = node
        .get_balance("0100000000000000000000000000000000000000")
        .await
        .expect("balance query should succeed without 0x prefix");

    let wei: u128 = result.parse().expect("balance should be parseable as u128");
    assert_eq!(wei, 1_000_000_000_000_000_000u128);
}

// ============================================================================
// Tests: Node start failure
// ============================================================================

#[tokio::test]
async fn test_node_start_failure_propagates_error() {
    let backend = Arc::new(FailingNodeBackend);
    let (node, _events) = make_node_service(backend);

    let result = node.start().await;
    match result {
        Err(AppError::Node(msg)) => {
            assert!(
                msg.contains("RocksDB"),
                "Error message should mention the cause: {}",
                msg
            );
        }
        other => panic!("Expected Node error from failing backend, got {:?}", other),
    }

    // Status should still be stopped after failed start
    assert!(
        !node.get_status().await.running,
        "Node should not be running after failed start"
    );
}

// ============================================================================
// Tests: Config chain_id wiring
// ============================================================================

#[tokio::test]
async fn test_node_status_chain_id_matches_config() {
    let (node, _events, _backend) = make_default_node_service();

    node.start().await.expect("node start should succeed");
    let status = node.get_status().await;
    let config = node.config.read().await;
    assert_eq!(
        status.chain_id, config.chain_id,
        "Node status chain_id should match config"
    );
}

// ============================================================================
// Tests: Reward address (returns None before wallet wiring)
// ============================================================================

#[tokio::test]
async fn test_reward_address_is_none_without_wallet() {
    let (node, _events, _backend) = make_default_node_service();

    assert!(
        node.get_reward_address().await.is_none(),
        "Reward address should be None before wallet is wired"
    );
}

// ============================================================================
// Tests: Connection status string logic (used by background thread)
// ============================================================================

#[tokio::test]
async fn test_connection_status_string_derivation() {
    let (node, _events, backend) = make_default_node_service();

    // Disconnected (not started)
    let status = node.get_status().await;
    let conn_str = derive_connection_string(&status);
    assert_eq!(conn_str, "Disconnected");

    // Started but no peers
    node.start().await.expect("node start should succeed");
    let status = node.get_status().await;
    let conn_str = derive_connection_string(&status);
    assert_eq!(conn_str, "Connecting to bootnode...");

    // With peers
    backend.set_peer_count(3);
    node.refresh_status().await;
    let status = node.get_status().await;
    let conn_str = derive_connection_string(&status);
    assert_eq!(conn_str, "Connected (3 peers)");
}

/// Mirrors the connection status string logic from main.rs background thread.
fn derive_connection_string(status: &NodeStatus) -> String {
    if status.peer_count > 0 {
        format!("Connected ({} peers)", status.peer_count)
    } else if status.running {
        "Connecting to bootnode...".to_string()
    } else {
        "Disconnected".to_string()
    }
}

// ============================================================================
// Tests: BlockSummary struct fields
// ============================================================================

#[tokio::test]
async fn test_block_summary_fields_for_ui_display() {
    let block = BlockSummary {
        hash: "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string(),
        height: 42,
        timestamp: 1711700000,
        tx_count: 3,
        selected_parent: "0x1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        blue_score: 40,
        proposer: String::new(),
    };

    // All fields used by the Slint UI
    let _hash: &str = &block.hash;
    let _height: u64 = block.height;
    let _timestamp: u64 = block.timestamp;
    let _tx_count: usize = block.tx_count;
    let _parent: &str = &block.selected_parent;
    let _score: u64 = block.blue_score;

    // Must be Clone + Debug for event loop crossing
    let _cloned = block.clone();
    let _debug = format!("{:?}", block);
}

// ============================================================================
// Tests: Block hash truncation logic (mirrors main.rs)
// ============================================================================

#[tokio::test]
async fn test_block_hash_truncation_for_display() {
    let long_hash = "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
    let truncated = truncate_hash(long_hash);
    assert_eq!(
        truncated, "0xabcdef12...7890",
        "Long hash should be truncated"
    );

    let short_hash = "0xabcdef";
    let result = truncate_hash(short_hash);
    assert_eq!(
        result, "0xabcdef",
        "Short hash should pass through unchanged"
    );
}

fn truncate_hash(hash: &str) -> String {
    if hash.len() > 18 {
        format!("{}...{}", &hash[..10], &hash[hash.len() - 4..])
    } else {
        hash.to_string()
    }
}

// ============================================================================
// Tests: Full lifecycle — start, produce blocks, refresh, stop
// ============================================================================

#[tokio::test]
async fn test_full_node_lifecycle() {
    let (node, events, backend) = make_default_node_service();
    let mut rx = events.subscribe();

    // 1. Start
    node.start().await.expect("node start should succeed");
    let start_event = rx.recv().await.expect("should receive start event");
    match start_event {
        AppEvent::NodeStatusChanged { running, .. } => assert!(running),
        other => panic!("Expected start event, got {:?}", other),
    }

    // 2. Simulate block production
    backend.set_block_height(10);
    backend.set_peer_count(2);
    node.refresh_status().await;

    let status = node.get_status().await;
    assert_eq!(status.block_height, 10);
    assert_eq!(status.peer_count, 2);

    // 3. More blocks
    backend.set_block_height(20);
    node.refresh_status().await;
    assert_eq!(node.get_status().await.block_height, 20);

    // 4. Stop
    node.stop().await.expect("node stop should succeed");
    assert!(!node.get_status().await.running);
}

// ============================================================================
// Tests: Event ordering preserved through lifecycle
// ============================================================================

#[tokio::test]
async fn test_events_ordered_through_lifecycle() {
    let (node, events, backend) = make_default_node_service();
    let mut rx = events.subscribe();

    // Start
    node.start().await.expect("start should succeed");

    // Multiple refreshes with increasing height
    for h in [5u64, 10, 15] {
        backend.set_block_height(h);
        node.refresh_status().await;
    }

    // Stop
    node.stop().await.expect("stop should succeed");

    // Collect all events
    let mut received = Vec::new();
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await
    {
        received.push(event);
    }

    // First event: start (running=true)
    match &received[0] {
        AppEvent::NodeStatusChanged { running, .. } => assert!(running),
        other => panic!("First event should be start, got {:?}", other),
    }

    // Last event: stop (running=false)
    match received.last().expect("should have events") {
        AppEvent::NodeStatusChanged { running, .. } => assert!(!running),
        other => panic!("Last event should be stop, got {:?}", other),
    }

    // At least start + some refreshes + stop
    assert!(
        received.len() >= 3,
        "Should have at least 3 events (start + refresh + stop), got {}",
        received.len()
    );
}

// ============================================================================
// Tests: Config access through node service
// ============================================================================

#[tokio::test]
async fn test_config_accessible_through_node_service() {
    let (node, _events, _backend) = make_default_node_service();

    let config = node.config.read().await;
    assert_eq!(config.chain_id, 40204);
    assert_eq!(config.network, "testnet");
    assert!(!config.bootnodes.is_empty());
    assert_eq!(config.rpc_port, citrate_desktop_app::DEFAULT_RPC_PORT);
    assert_eq!(config.p2p_port, 30304);
}

// ============================================================================
// Tests: Multiple node service instances are independent
// ============================================================================

#[tokio::test]
async fn test_multiple_node_instances_independent() {
    let (node1, _events1, backend1) = make_default_node_service();
    let (node2, _events2, backend2) = make_default_node_service();

    node1.start().await.expect("node1 start should succeed");
    backend1.set_block_height(100);
    node1.refresh_status().await;

    // node2 should still be at default state
    assert!(!node2.get_status().await.running);
    assert_eq!(node2.get_status().await.block_height, 0);

    // Start node2 with different height
    node2.start().await.expect("node2 start should succeed");
    backend2.set_block_height(50);
    node2.refresh_status().await;

    // They should be independent
    assert_eq!(node1.get_status().await.block_height, 100);
    assert_eq!(node2.get_status().await.block_height, 50);
}
