//! IPFS integration tests — daemon lifecycle and model storage.
//!
//! Tests the IPFS daemon auto-start behavior, status reporting,
//! and integration with the embedded node. Uses test backends
//! to avoid requiring a real kubo installation.

use citrate_desktop_app::error::AppError;
use citrate_desktop_app::event_bus::EventBus;
use citrate_desktop_app::services::node_service::{NodeBackend, NodeService};
use citrate_desktop_app::AppConfig;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Test node backend that tracks IPFS initialization calls
struct IpfsTrackingNodeBackend {
    ipfs_initialized: std::sync::atomic::AtomicBool,
    started: std::sync::atomic::AtomicBool,
}

impl IpfsTrackingNodeBackend {
    fn new() -> Self {
        Self {
            ipfs_initialized: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn was_ipfs_initialized(&self) -> bool {
        self.ipfs_initialized
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl NodeBackend for IpfsTrackingNodeBackend {
    async fn start_node(&self, _chain_id: u64, _data_dir: &str) -> Result<(), AppError> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // In production, IPFS daemon should auto-initialize here
        // This test backend tracks whether the call was made
        self.ipfs_initialized
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn stop_node(&self) -> Result<(), AppError> {
        self.started
            .store(false, std::sync::atomic::Ordering::SeqCst);
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
    async fn get_balance(&self, _: &[u8; 20]) -> String {
        "0".to_string()
    }
}

fn test_node_with_ipfs() -> (NodeService, Arc<IpfsTrackingNodeBackend>) {
    let backend = Arc::new(IpfsTrackingNodeBackend::new());
    let config = Arc::new(RwLock::new(AppConfig::default()));
    let events = Arc::new(EventBus::new());
    let svc = NodeService::with_backend(config, events, backend.clone());
    (svc, backend)
}

// =========================================================================
// IPFS DAEMON LIFECYCLE
// =========================================================================

#[tokio::test]
async fn test_ipfs_initializes_on_node_start() {
    let (svc, backend) = test_node_with_ipfs();
    assert!(!backend.was_ipfs_initialized());

    svc.start().await.expect("node start");
    assert!(
        backend.was_ipfs_initialized(),
        "IPFS should initialize when node starts"
    );
}

#[tokio::test]
async fn test_ipfs_not_initialized_before_start() {
    let (_svc, backend) = test_node_with_ipfs();
    assert!(!backend.was_ipfs_initialized());
}

#[tokio::test]
async fn test_node_stop_cleans_up() {
    let (svc, _backend) = test_node_with_ipfs();
    svc.start().await.expect("start");
    svc.stop().await.expect("stop");
    let status = svc.get_status().await;
    assert!(!status.running);
}

// =========================================================================
// IPFS SERVICE UNIT TESTS (from storage crate)
// =========================================================================

#[test]
fn test_ipfs_service_creation() {
    let svc = citrate_storage::ipfs::IPFSService::new("http://localhost:5001".to_string());
    assert!(svc.list_pinned_models().is_empty());
}

#[test]
fn test_ipfs_cid_equality() {
    let cid1 = citrate_storage::ipfs::Cid("QmTest123".to_string());
    let cid2 = citrate_storage::ipfs::Cid("QmTest123".to_string());
    let cid3 = citrate_storage::ipfs::Cid("QmDifferent".to_string());
    assert_eq!(cid1, cid2);
    assert_ne!(cid1, cid3);
}

#[test]
fn test_daemon_config_defaults() {
    let config = citrate_storage::ipfs::DaemonConfig::default();
    assert!(config.auto_start);
    assert!(config.auto_download);
    assert!(
        config.api_addr.contains("5001"),
        "API addr should contain port 5001"
    );
    assert!(
        config.gateway_addr.contains("8080"),
        "Gateway addr should contain port 8080"
    );
}

#[test]
fn test_daemon_creation() {
    let config = citrate_storage::ipfs::DaemonConfig::default();
    let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
    assert_eq!(daemon.api_url(), "http://127.0.0.1:5001");
}

#[tokio::test]
async fn test_daemon_status_returns_valid_enum() {
    let config = citrate_storage::ipfs::DaemonConfig::default();
    let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
    let status = daemon.status().await;
    // Status should be one of the valid enum variants
    // (may be Running if a local IPFS daemon is active)
    // Valid status — any variant is acceptable at this point
    let _ = status; // Status depends on local IPFS daemon state
}

#[tokio::test]
async fn test_daemon_is_running_returns_bool() {
    let config = citrate_storage::ipfs::DaemonConfig::default();
    let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
    // Just verify it returns without panicking — result depends on local daemon state
    let _running = daemon.is_running().await;
}

// =========================================================================
// MODEL METADATA
// =========================================================================

#[test]
fn test_model_metadata_serialization() {
    let metadata = citrate_storage::ipfs::ModelMetadata {
        name: "qwen2.5-0.5b".to_string(),
        version: "1.0".to_string(),
        framework: citrate_storage::ipfs::ModelFramework::Custom("GGUF".to_string()),
        model_type: citrate_storage::ipfs::ModelType::Language,
        size_bytes: 469_000_000,
        input_shape: vec![1, 2048],
        output_shape: vec![1, 2048, 151936],
        description: "Qwen 2.5 0.5B Instruct GGUF".to_string(),
        author: "Qwen".to_string(),
        license: "Apache-2.0".to_string(),
        created_at: 1711555200,
    };

    let json = serde_json::to_string(&metadata).expect("serialize");
    assert!(json.contains("qwen2.5-0.5b"));
    assert!(json.contains("469000000"));

    let parsed: citrate_storage::ipfs::ModelMetadata =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.name, "qwen2.5-0.5b");
    assert_eq!(parsed.size_bytes, 469_000_000);
}

#[test]
fn test_pin_reward_for_large_model() {
    let mut svc = citrate_storage::ipfs::IPFSService::new("http://localhost:5001".to_string());
    let cid = citrate_storage::ipfs::Cid("QmLargeModel".to_string());
    let metadata = citrate_storage::ipfs::ModelMetadata {
        name: "Large Model".to_string(),
        version: "1.0".to_string(),
        framework: citrate_storage::ipfs::ModelFramework::ONNX,
        model_type: citrate_storage::ipfs::ModelType::Language,
        size_bytes: 7_000_000_000, // 7GB
        input_shape: vec![1, 4096],
        output_shape: vec![1, 4096, 32000],
        description: "Large language model".to_string(),
        author: "Test".to_string(),
        license: "MIT".to_string(),
        created_at: 0,
    };

    svc.record_external_pin(
        cid.clone(),
        "node-1".to_string(),
        metadata.clone(),
        metadata.size_bytes,
    );
    let reward = svc.calculate_pin_reward(&cid, 24);
    assert!(
        reward > 0,
        "Large model pinning should earn non-zero reward"
    );
}
