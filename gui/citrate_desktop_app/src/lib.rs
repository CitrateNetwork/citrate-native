//! Citrate Desktop Application Core
//!
//! Headless orchestration layer for the Citrate desktop GUI. This crate owns:
//! - Typed service interfaces for node, wallet, models, and chain queries
//! - View model definitions consumed by the UI layer
//! - Event bus for background-to-UI communication
//! - Application state lifecycle (init, start, stop)
//!
//! This crate has NO dependency on any window framework (Tauri, Slint, egui).
//! It compiles and tests independently.

pub mod error;
pub mod event_bus;
pub mod ports;
pub mod services;
pub mod trail;
pub mod view_models;

use std::sync::Arc;
use tokio::sync::RwLock;

/// Top-level application state shared across all services.
///
/// Constructed once at startup. The UI layer receives an `Arc<AppCore>`
/// and calls typed methods — never raw IPC strings.
pub struct AppCore {
    /// Node lifecycle and chain queries
    pub node: Arc<services::NodeService>,
    /// Wallet operations
    pub wallet: Arc<services::WalletService>,
    // (editor / file_explorer / git / compiler / terminal services
    // were retired in P960-H along with the Contracts surface that
    // consumed them. If a future panel needs them, lift them back
    // from git history rather than carrying dead infrastructure.)
    /// MCP server — capability boundary for external agent runtimes
    /// (P960-J). Shared with McpHostService; direct handle kept here
    /// so future panels can register grants or tools programmatically.
    pub mcp: Arc<citrate_agent_core::mcp_server::McpServer>,
    /// In-GUI agent session's active policy (P960-K T1-2). Defaults
    /// to Guided (every high-risk tool needs explicit approval).
    /// User can flip to ReadOnly from the Operations panel to lock
    /// mutation tools out entirely without locking the whole wallet.
    pub session_policy: Arc<RwLock<citrate_agent_core::canonical::PolicyProfile>>,
    /// MCP HTTP host — serves JSON-RPC 2.0 over 127.0.0.1:{mcp_port}
    /// so external agent runtimes (Hermes et al.) can discover and
    /// connect. Status feeds the Operations panel.
    pub mcp_host: Arc<services::mcp_host::McpHostService>,
    /// AI chat — conversation with on-chain AI via citrate_chatCompletion
    pub chat: Arc<services::ChatService>,
    /// Model registry — browse, deploy, and run inference on AI models
    pub models: Arc<services::ModelService>,
    /// Block/transaction explorer — query blocks, txs, DAG structure
    pub blocks: Arc<services::BlockService>,
    /// Learning center — pools, staking, training, earnings
    pub learning: Arc<services::LearningService>,
    /// Compute marketplace — jobs, providers, GPU
    pub compute: Arc<services::ComputeService>,
    /// Event bus for background → UI notifications
    pub events: Arc<event_bus::EventBus>,
    /// Trail recorder — captures runtime events as canonical TrailEvents
    pub trail: Arc<trail::TrailRecorder>,
    /// Pending approval store — manages tool approval requests with timeout
    pub approvals: Arc<citrate_agent_core::delegation::PendingApprovalStore>,
    /// Agent tool registry — all registered tools for chat function calling
    pub tool_registry: Arc<citrate_agent_core::tool::ToolRegistry>,
    /// Application-wide configuration
    pub config: Arc<RwLock<AppConfig>>,
}

/// Application configuration (persisted across sessions)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppConfig {
    pub network: String,
    pub chain_id: u64,
    pub data_dir: String,
    pub rpc_port: u16,
    pub p2p_port: u16,
    /// P960-J: port the MCP host binds on 127.0.0.1. 9600 by default.
    /// Kept at 0 in serialized form means "use DEFAULT_MCP_PORT" —
    /// older configs without this field migrate cleanly.
    #[serde(default)]
    pub mcp_port: u16,
    pub bootnodes: Vec<String>,
    pub theme: String,
    /// AI provider API keys (provider name → encrypted key)
    #[serde(default)]
    pub ai_keys: std::collections::HashMap<String, String>,
    /// AI provider priority order (first = highest priority)
    #[serde(default = "default_ai_priority")]
    pub ai_priority: Vec<String>,
    /// Logseq graph path
    #[serde(default = "default_logseq_path")]
    pub logseq_graph_path: String,
    /// Whether logseq integration is enabled
    #[serde(default)]
    pub logseq_enabled: bool,
    /// Whether auto-journal is enabled
    #[serde(default)]
    pub auto_journal: bool,
    /// Whether on-chain anchoring is enabled
    #[serde(default)]
    pub on_chain_anchoring: bool,
    /// Integration tokens (integration name → token)
    #[serde(default)]
    pub integration_tokens: std::collections::HashMap<String, String>,
}

fn default_ai_priority() -> Vec<String> {
    vec!["local".to_string(), "openai".to_string(), "anthropic".to_string()]
}

fn default_logseq_path() -> String {
    dirs::data_local_dir()
        .map(|d| d.join("citrate").join("logseq").join("graphs").join("default").to_string_lossy().to_string())
        .unwrap_or_else(|| "~/.citrate/logseq/graphs/default".to_string())
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            network: "testnet".to_string(),
            chain_id: 40204,
            data_dir: data_dir_for_network("testnet"),
            rpc_port: 18545,
            p2p_port: 30304,
            mcp_port: services::mcp_host::DEFAULT_MCP_PORT,
            bootnodes: vec![
                "159.65.227.42:30303".to_string(),
            ],
            theme: "dark".to_string(),
            ai_keys: std::collections::HashMap::new(),
            ai_priority: default_ai_priority(),
            logseq_graph_path: default_logseq_path(),
            logseq_enabled: false,
            auto_journal: false,
            on_chain_anchoring: false,
            integration_tokens: std::collections::HashMap::new(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn default_data_dir() -> String {
    dirs::data_local_dir()
        .map(|d| d.join("citrate-gui").to_string_lossy().to_string())
        .unwrap_or_else(|| ".citrate-gui".to_string())
}

/// Return a network-specific data directory so devnet and testnet don't share
/// the same RocksDB instance.  Layout:
///   devnet  → <base>/devnet   (e.g. ~/.local/share/citrate-gui/devnet)
///   testnet → <base>/testnet  (e.g. ~/.local/share/citrate-gui/testnet)
pub fn data_dir_for_network(network: &str) -> String {
    let base = dirs::data_local_dir()
        .map(|d| d.join("citrate-gui"))
        .unwrap_or_else(|| std::path::PathBuf::from(".citrate-gui"));
    let suffix = match network.to_lowercase().as_str() {
        "testnet" => "testnet",
        _ => "devnet",
    };
    base.join(suffix).to_string_lossy().to_string()
}

/// Get the correct chain_id for a network name.
///
/// Testnet beta (40204) is the only live Citrate network. Any non-mainnet
/// alias resolves to 40204.
pub fn chain_id_for_network(network: &str) -> u64 {
    match network.to_lowercase().as_str() {
        "mainnet" => 1, // Reserved
        _ => 40204,
    }
}

impl AppConfig {
    /// Load config from disk, or return default.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    match serde_json::from_str::<AppConfig>(&contents) {
                        Ok(mut config) => {
                            // Migrate stale configs: if network/chain_id are mismatched, fix them
                            let expected_chain_id = chain_id_for_network(&config.network);
                            if config.chain_id != expected_chain_id {
                                tracing::warn!(
                                    "Config migration: network='{}' had chain_id={}, expected {}. Fixing.",
                                    config.network, config.chain_id, expected_chain_id
                                );
                                // If chain_id is testnet but label says devnet, fix the label
                                if config.chain_id == 40204 && config.network == "devnet" {
                                    config.network = "testnet".to_string();
                                    config.data_dir = data_dir_for_network("testnet");
                                } else {
                                    config.chain_id = expected_chain_id;
                                }
                                // Persist the fix
                                if let Err(e) = config.save() {
                                    tracing::warn!("Failed to save migrated config: {}", e);
                                }
                            }
                            tracing::info!("Loaded config from {:?}", path);
                            return config;
                        }
                        Err(e) => tracing::warn!("Config parse error: {}, using default", e),
                    }
                }
                Err(e) => tracing::warn!("Config read error: {}, using default", e),
            }
        }
        Self::default()
    }

    /// Save config to disk.
    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        std::fs::write(&path, json)?;
        tracing::info!("Config saved to {:?}", path);
        Ok(())
    }

    fn config_path() -> std::path::PathBuf {
        dirs::data_local_dir()
            .map(|d| d.join("citrate-gui").join("config.json"))
            .unwrap_or_else(|| std::path::PathBuf::from(".citrate-gui/config.json"))
    }
}

impl AppCore {
    /// Create a new application core with default configuration.
    pub fn new() -> Self {
        let loaded = AppConfig::load();
        let rpc_url = format!("http://127.0.0.1:{}", loaded.rpc_port);
        let config = Arc::new(RwLock::new(loaded));
        let events = Arc::new(event_bus::EventBus::new());
        let node = Arc::new(services::NodeService::new(
            config.clone(),
            events.clone(),
        ));
        let wallet = Arc::new(services::WalletService::new(events.clone()));
        // Chat: local-first, private-by-default
        // Ollama primary (fast local GPU, localhost:11434) → node GGUF RPC fallback (slow CPU)
        // Never sends data externally unless the user explicitly configures an API key.
        let chat = {
            use services::chat_service::{OpenAICompatibleBackend, RpcChatBackend, FallbackChatBackend};
            let ollama = Arc::new(OpenAICompatibleBackend::new(
                "http://localhost:11434/v1/chat/completions", ""
            ));
            let node_rpc = Arc::new(RpcChatBackend::new(&rpc_url));
            let backend = Arc::new(FallbackChatBackend::new(ollama).with_fallback(node_rpc));
            // Model name will be updated by detect_local_backend() in the GUI layer.
            // Default to a safe placeholder; the real model is set after async detection.
            Arc::new(services::ChatService::with_backend_and_model(
                events.clone(),
                backend,
                "auto-detect",
            ))
        };
        let models = Arc::new(services::ModelService::new(events.clone(), &rpc_url));
        let blocks = Arc::new(services::BlockService::new(events.clone(), &rpc_url));
        let learning = Arc::new(services::LearningService::new(events.clone(), &rpc_url));
        let compute = Arc::new(services::ComputeService::new(events.clone(), &rpc_url));

        // Pending approval store — manages tool approval requests
        let approvals = Arc::new(citrate_agent_core::delegation::PendingApprovalStore::new());

        // Agent tool registry — register available tools for chat function calling
        let tool_registry = Arc::new(citrate_agent_core::tool::ToolRegistry::new());

        // P960-J: MCP server + HTTP host for external agent runtimes.
        // Constructed here but NOT bound — AppCore::start() (the
        // async init path) calls mcp_host.start(port) so bind errors
        // surface on startup rather than in the constructor.
        let mcp = Arc::new(citrate_agent_core::mcp_server::McpServer::new(tool_registry.clone()));
        let mcp_host = Arc::new(services::mcp_host::McpHostService::new(mcp.clone()));
        let session_policy = Arc::new(RwLock::new(
            citrate_agent_core::canonical::PolicyProfile::Guided
        ));

        // Trail recorder — subscribes to event bus and records canonical TrailEvents.
        // LogSeq path from config (if enabled).
        let logseq_path = {
            let cfg = config.try_read().map(|c| {
                if c.logseq_enabled {
                    Some(c.logseq_graph_path.clone())
                } else {
                    None
                }
            }).unwrap_or(None);
            cfg
        };
        let trail = Arc::new(trail::TrailRecorder::new(
            &uuid::Uuid::new_v4().to_string(),
            logseq_path,
        ));
        // Start the event bus subscriber in the background (if tokio runtime is available)
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let trail_sub = trail.clone();
            let events_sub = events.clone();
            handle.spawn(async move {
                let mut rx = events_sub.subscribe();
                while let Ok(event) = rx.recv().await {
                    trail_sub.record_app_event(&event).await;
                }
            });
        }

        Self {
            node,
            wallet,
            mcp,
            mcp_host,
            session_policy,
            chat,
            models,
            blocks,
            learning,
            compute,
            events,
            trail,
            approvals,
            tool_registry,
            config,
        }
    }

}

impl Default for AppCore {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore {
    /// Start the application (init node, load wallet, begin polling)
    pub async fn start(&self) -> Result<(), error::AppError> {
        tracing::info!("AppCore starting");
        self.wallet.load_from_disk().await?;
        self.node.start().await?;
        tracing::info!("AppCore started");
        Ok(())
    }

    /// Stop the application cleanly
    pub async fn stop(&self) -> Result<(), error::AppError> {
        tracing::info!("AppCore stopping");
        self.node.stop().await?;
        tracing::info!("AppCore stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.chain_id, 40204);
        assert_eq!(config.network, "testnet");
        assert_eq!(config.rpc_port, 18545);
        assert_eq!(config.p2p_port, 30304);
        assert!(!config.bootnodes.is_empty());
        assert_eq!(config.theme, "dark");
    }

    #[test]
    fn test_app_core_creates() {
        let core = AppCore::new();
        assert!(Arc::strong_count(&core.events) >= 1);
    }

    #[test]
    fn test_app_core_shared_references() {
        let core = AppCore::new();
        // EventBus is shared between AppCore, NodeService, and WalletService
        assert!(Arc::strong_count(&core.events) >= 3);
    }

    #[test]
    fn test_config_serialization() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).expect("serialization succeeded");
        assert!(json.contains("40204"));
        assert!(json.contains("testnet"));
    }

    #[test]
    fn test_config_deserialization() {
        let json = r#"{"network":"testnet","chain_id":40204,"data_dir":"/tmp/test","rpc_port":18545,"p2p_port":30304,"bootnodes":[],"theme":"light"}"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialization succeeded");
        assert_eq!(config.network, "testnet");
        assert_eq!(config.theme, "light");
        assert!(config.bootnodes.is_empty());
    }

    #[test]
    fn test_config_roundtrip() {
        let original = AppConfig::default();
        let json = serde_json::to_string(&original).expect("serialization succeeded");
        let deserialized: AppConfig = serde_json::from_str(&json).expect("test assertion");
        assert_eq!(original.chain_id, deserialized.chain_id);
        assert_eq!(original.network, deserialized.network);
        assert_eq!(original.rpc_port, deserialized.rpc_port);
        assert_eq!(original.p2p_port, deserialized.p2p_port);
        assert_eq!(original.theme, deserialized.theme);
    }

    #[test]
    fn test_config_clone() {
        let config = AppConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.chain_id, config.chain_id);
        assert_eq!(cloned.network, config.network);
    }

    #[test]
    fn test_config_debug() {
        let config = AppConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("chain_id"));
        assert!(debug.contains("40204"));
    }

    #[test]
    fn test_default_data_dir_not_empty() {
        let dir = default_data_dir();
        assert!(!dir.is_empty());
    }

    #[test]
    fn test_default_data_dir_contains_citrate() {
        let dir = default_data_dir();
        assert!(dir.contains("citrate"), "Data dir should contain 'citrate': {}", dir);
    }

    #[tokio::test]
    async fn test_app_core_start_stop() {
        // Use test backend to avoid RocksDB lock contention in parallel tests
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(event_bus::EventBus::new());
        let node = Arc::new(services::NodeService::with_backend(
            config.clone(),
            events.clone(),
            Arc::new(services::node_service::TestNodeBackend),
        ));
        // Start/stop cycle works with test backend
        node.start().await.expect("start succeeded");
        assert!(node.get_status().await.running);
        node.stop().await.expect("stop succeeded");
        assert!(!node.get_status().await.running);
    }

    #[tokio::test]
    async fn test_app_core_wallet_first_run() {
        let core = AppCore::new();
        assert!(core.wallet.is_first_run().await);
    }

    #[tokio::test]
    async fn test_app_core_config_access() {
        let core = AppCore::new();
        let config = core.config.read().await;
        assert_eq!(config.chain_id, 40204);
    }

    #[tokio::test]
    async fn test_app_core_event_subscription() {
        // Use test backend to avoid RocksDB lock contention
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let events = Arc::new(event_bus::EventBus::new());
        let mut rx = events.subscribe();
        let node = Arc::new(services::NodeService::with_backend(
            config.clone(),
            events.clone(),
            Arc::new(services::node_service::TestNodeBackend),
        ));
        node.start().await.expect("start succeeded");

        let event = rx.recv().await.expect("event received");
        match event {
            crate::event_bus::AppEvent::NodeStatusChanged { running, .. } => assert!(running),
            _ => panic!("Expected NodeStatusChanged from start"),
        }
    }

    #[test]
    fn test_multiple_app_cores_independent() {
        let core1 = AppCore::new();
        let core2 = AppCore::new();
        // Each has its own event bus — no shared state
        assert!(Arc::strong_count(&core1.events) >= 1);
        assert!(Arc::strong_count(&core2.events) >= 1);
        // Different Arc instances
        assert!(!Arc::ptr_eq(&core1.events, &core2.events));
    }

    #[test]
    fn test_config_ports() {
        let config = AppConfig::default();
        assert_ne!(config.rpc_port, config.p2p_port, "RPC and P2P ports must differ");
    }

    #[test]
    fn test_config_bootnodes_not_empty() {
        let config = AppConfig::default();
        assert!(!config.bootnodes.is_empty(), "Default config must include bootnode");
    }
}
