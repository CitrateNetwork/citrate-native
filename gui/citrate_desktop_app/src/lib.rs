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
    /// Code editor — rope buffer, syntax highlighting, undo/redo
    pub editor: Arc<services::EditorService>,
    /// File explorer — directory tree, gitignore-aware traversal
    pub file_explorer: Arc<services::FileExplorerService>,
    /// Git operations — status, diff, commit, branch, push/pull
    pub git: Arc<services::GitService>,
    /// Solidity compiler — forge/solc integration, diagnostics
    pub compiler: Arc<services::CompilerService>,
    /// Terminal — PTY sessions, ANSI parsing, cell grid
    pub terminal: Arc<services::TerminalService>,
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
/// Devnet = 1337 (local), Testnet = 40204 (public).
pub fn chain_id_for_network(network: &str) -> u64 {
    match network.to_lowercase().as_str() {
        "devnet" => 1337,
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
                        Ok(config) => {
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
        let editor = Arc::new(services::EditorService::new(events.clone()));
        let file_explorer = Arc::new(services::FileExplorerService::new(events.clone()));
        let git = Arc::new(services::GitService::new(events.clone()));
        let compiler = Arc::new(services::CompilerService::new(events.clone()));
        let terminal = Arc::new(services::TerminalService::new(events.clone()));
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

        Self {
            node,
            wallet,
            editor,
            file_explorer,
            git,
            compiler,
            terminal,
            chat,
            models,
            blocks,
            learning,
            compute,
            events,
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
