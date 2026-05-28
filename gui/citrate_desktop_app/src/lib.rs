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

use crate::ports::{SecretStore, SystemSecretStore};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

const SECRET_MARKER_PREFIX: &str = "keyring:v1:";
const SECRET_KIND_AI: &str = "ai";
const SECRET_KIND_INTEGRATION: &str = "integration";

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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    /// AI provider key markers (provider name -> OS-keychain reference).
    ///
    /// Legacy configs may contain plaintext here; `AppConfig::load` migrates
    /// them to the OS credential store and rewrites this map with markers.
    #[serde(default)]
    pub ai_keys: HashMap<String, String>,
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
    /// Integration token markers (integration name -> OS-keychain reference).
    ///
    /// Legacy configs may contain plaintext here; `AppConfig::load` migrates
    /// them to the OS credential store and rewrites this map with markers.
    #[serde(default)]
    pub integration_tokens: HashMap<String, String>,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("network", &self.network)
            .field("chain_id", &self.chain_id)
            .field("data_dir", &self.data_dir)
            .field("rpc_port", &self.rpc_port)
            .field("p2p_port", &self.p2p_port)
            .field("mcp_port", &self.mcp_port)
            .field("bootnodes", &self.bootnodes)
            .field("theme", &self.theme)
            .field("ai_keys", &redacted_secret_keys(&self.ai_keys))
            .field("ai_priority", &self.ai_priority)
            .field("logseq_graph_path", &self.logseq_graph_path)
            .field("logseq_enabled", &self.logseq_enabled)
            .field("auto_journal", &self.auto_journal)
            .field("on_chain_anchoring", &self.on_chain_anchoring)
            .field(
                "integration_tokens",
                &redacted_secret_keys(&self.integration_tokens),
            )
            .finish()
    }
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
            // Current testnet-beta bootnodes (mirror of citrate-chain
            // node/config/testnet-beta.toml). boot{1,2,3} are discovery; the
            // sequencer (rpc.citrate.ai) is the block source that drives sync.
            // Hostnames resolve via the shared citrate_network::resolve_bootnode.
            bootnodes: vec![
                "noise_f356d3ebb07371eaad371b3960272f9d58fc457cde409c34549ef03776b78141@boot1.citrate.ai:30303".to_string(),
                "noise_4ed281386422f6a65b92d8760d24baa82bb1b476e9dd3e21d5a070d026802c07@boot2.citrate.ai:30303".to_string(),
                "noise_2b4924671e0babc9f52eb1695c72141a9c639e17ad95a2a2d2a715eae34a420e@boot3.citrate.ai:30303".to_string(),
                "noise_6ee549718d522c9ccc122585dfedef72aea8df24f4a2bb0264e7658a91118b4a@rpc.citrate.ai:30303".to_string(),
            ],
            theme: "dark".to_string(),
            ai_keys: HashMap::new(),
            ai_priority: default_ai_priority(),
            logseq_graph_path: default_logseq_path(),
            logseq_enabled: false,
            auto_journal: false,
            on_chain_anchoring: false,
            integration_tokens: HashMap::new(),
        }
    }
}

fn redacted_secret_keys(map: &HashMap<String, String>) -> Vec<&str> {
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

fn secret_marker(kind: &str, name: &str) -> String {
    format!("{}{}:{}", SECRET_MARKER_PREFIX, kind, hex::encode(name.as_bytes()))
}

fn secret_store_key(kind: &str, name: &str) -> String {
    format!("{}:{}", kind, hex::encode(name.as_bytes()))
}

fn is_secret_marker(value: &str) -> bool {
    value.starts_with(SECRET_MARKER_PREFIX)
}

fn io_secret_error(action: &str, err: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, format!("{action}: {err}"))
}

fn secret_value_requires_storage(value: &str) -> bool {
    !value.is_empty() && !is_secret_marker(value)
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
        Self::load_from_path_with_secret_store(&Self::config_path(), &SystemSecretStore::new())
    }

    pub fn load_from_path_with_secret_store(path: &Path, secret_store: &dyn SecretStore) -> Self {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    match serde_json::from_str::<AppConfig>(&contents) {
                        Ok(mut config) => {
                            let mut needs_save = false;
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
                                needs_save = true;
                            }

                            match config.migrate_plaintext_secrets(secret_store) {
                                Ok(changed) => needs_save |= changed,
                                Err(e) => {
                                    tracing::warn!(
                                        "Config migration: failed to move plaintext secrets to OS keychain: {}. \
                                         Dropping plaintext values from config; re-enter affected keys in Settings.",
                                        e
                                    );
                                    config.drop_plaintext_secret_values();
                                    needs_save = true;
                                }
                            }

                            if needs_save {
                                if let Err(e) = config.write_sanitized_to_path(path) {
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
        self.save_to_path_with_secret_store(&Self::config_path(), &SystemSecretStore::new())
    }

    pub fn save_to_path_with_secret_store(
        &self,
        path: &Path,
        secret_store: &dyn SecretStore,
    ) -> Result<(), std::io::Error> {
        let mut sanitized = self.clone();
        sanitized
            .migrate_plaintext_secrets(secret_store)
            .map_err(|e| io_secret_error("failed to store config secrets", e))?;
        sanitized.write_sanitized_to_path(path)
    }

    fn write_sanitized_to_path(&self, path: &Path) -> Result<(), std::io::Error> {
        debug_assert!(
            !self.contains_plaintext_secret_values(),
            "AppConfig::write_sanitized_to_path called with plaintext secrets"
        );
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

    pub fn set_ai_key(&mut self, provider: &str, key: &str) -> Result<(), std::io::Error> {
        self.set_ai_key_with_secret_store(provider, key, &SystemSecretStore::new())
    }

    pub fn set_ai_key_with_secret_store(
        &mut self,
        provider: &str,
        key: &str,
        secret_store: &dyn SecretStore,
    ) -> Result<(), std::io::Error> {
        let store_key = secret_store_key(SECRET_KIND_AI, provider);
        secret_store
            .set_secret(&store_key, key)
            .map_err(|e| io_secret_error("failed to store AI provider key", e))?;
        self.ai_keys
            .insert(provider.to_string(), secret_marker(SECRET_KIND_AI, provider));
        Ok(())
    }

    pub fn get_ai_key_with_secret_store(
        &self,
        provider: &str,
        secret_store: &dyn SecretStore,
    ) -> Option<String> {
        match self.ai_keys.get(provider) {
            Some(marker) if is_secret_marker(marker) => {
                secret_store.get_secret(&secret_store_key(SECRET_KIND_AI, provider))
            }
            _ => None,
        }
    }

    pub fn set_integration_token(
        &mut self,
        name: &str,
        token: &str,
    ) -> Result<(), std::io::Error> {
        self.set_integration_token_with_secret_store(
            name,
            token,
            &SystemSecretStore::new(),
        )
    }

    pub fn set_integration_token_with_secret_store(
        &mut self,
        name: &str,
        token: &str,
        secret_store: &dyn SecretStore,
    ) -> Result<(), std::io::Error> {
        let store_key = secret_store_key(SECRET_KIND_INTEGRATION, name);
        secret_store
            .set_secret(&store_key, token)
            .map_err(|e| io_secret_error("failed to store integration token", e))?;
        self.integration_tokens.insert(
            name.to_string(),
            secret_marker(SECRET_KIND_INTEGRATION, name),
        );
        Ok(())
    }

    pub fn get_integration_token_with_secret_store(
        &self,
        name: &str,
        secret_store: &dyn SecretStore,
    ) -> Option<String> {
        match self.integration_tokens.get(name) {
            Some(marker) if is_secret_marker(marker) => secret_store
                .get_secret(&secret_store_key(SECRET_KIND_INTEGRATION, name)),
            _ => None,
        }
    }

    fn migrate_plaintext_secrets(
        &mut self,
        secret_store: &dyn SecretStore,
    ) -> Result<bool, String> {
        let mut changed = false;
        changed |= migrate_plaintext_secret_map(
            &mut self.ai_keys,
            SECRET_KIND_AI,
            secret_store,
        )?;
        changed |= migrate_plaintext_secret_map(
            &mut self.integration_tokens,
            SECRET_KIND_INTEGRATION,
            secret_store,
        )?;
        Ok(changed)
    }

    fn contains_plaintext_secret_values(&self) -> bool {
        self.ai_keys
            .values()
            .any(|value| secret_value_requires_storage(value))
            || self
                .integration_tokens
                .values()
                .any(|value| secret_value_requires_storage(value))
    }

    fn drop_plaintext_secret_values(&mut self) {
        self.ai_keys
            .retain(|_, value| value.is_empty() || is_secret_marker(value));
        self.integration_tokens
            .retain(|_, value| value.is_empty() || is_secret_marker(value));
    }
}

fn migrate_plaintext_secret_map(
    map: &mut HashMap<String, String>,
    kind: &str,
    secret_store: &dyn SecretStore,
) -> Result<bool, String> {
    let mut changed = false;
    let entries: Vec<(String, String)> = map
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();

    for (name, value) in entries {
        if value.is_empty() {
            continue;
        }
        if is_secret_marker(&value) {
            continue;
        }
        let store_key = secret_store_key(kind, &name);
        secret_store.set_secret(&store_key, &value)?;
        map.insert(name.clone(), secret_marker(kind, &name));
        changed = true;
    }

    Ok(changed)
}

impl AppCore {
    /// Create a new application core with default configuration.
    pub fn new() -> Self {
        let loaded = AppConfig::load();
        // RPC URL — the embedded node DELIBERATELY does not serve HTTP JSON-RPC
        // (only P2P sync), so a local-port URL is wrong by design for every
        // network except an isolated devnet. Testnet/anything-else points at
        // the public sequencer (https://rpc.citrate.ai), matching the same
        // network-aware selector used by the GUI's `active_rpc_url` and the
        // wallet-core default. This fixes the chat/model/block/learning/compute
        // services that previously failed with "Chat RPC failed: error sending
        // request for url(…127.0.0.1:18545)".
        let rpc_url = match loaded.network.as_str() {
            "devnet" => format!("http://127.0.0.1:{}", loaded.rpc_port),
            _ => "https://rpc.citrate.ai".to_string(),
        };
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
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestSecretStore {
        data: Mutex<HashMap<String, String>>,
        fail_writes: bool,
    }

    impl TestSecretStore {
        fn failing() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
                fail_writes: true,
            }
        }
    }

    impl SecretStore for TestSecretStore {
        fn get_secret(&self, key: &str) -> Option<String> {
            self.data.lock().expect("secret store mutex").get(key).cloned()
        }

        fn set_secret(&self, key: &str, value: &str) -> Result<(), String> {
            if self.fail_writes {
                return Err("injected secret-store failure".to_string());
            }
            self.data
                .lock()
                .expect("secret store mutex")
                .insert(key.to_string(), value.to_string());
            Ok(())
        }

        fn delete_secret(&self, key: &str) -> Result<(), String> {
            self.data.lock().expect("secret store mutex").remove(key);
            Ok(())
        }

        fn has_secret(&self, key: &str) -> bool {
            self.data.lock().expect("secret store mutex").contains_key(key)
        }
    }

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
    fn test_t0_03_config_save_moves_plaintext_secrets_out_of_json() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("config.json");
        let store = TestSecretStore::default();
        let mut config = AppConfig::default();
        config
            .ai_keys
            .insert("openai".to_string(), "sk-live-test-secret".to_string());
        config.integration_tokens.insert(
            "huggingface".to_string(),
            "hf_live_test_secret".to_string(),
        );

        config
            .save_to_path_with_secret_store(&path, &store)
            .expect("save sanitized config");
        let raw = std::fs::read_to_string(&path).expect("read config");

        assert!(!raw.contains("sk-live-test-secret"));
        assert!(!raw.contains("hf_live_test_secret"));
        assert!(raw.contains(SECRET_MARKER_PREFIX));
        assert_eq!(
            store.get_secret(&secret_store_key(SECRET_KIND_AI, "openai")),
            Some("sk-live-test-secret".to_string())
        );
        assert_eq!(
            store.get_secret(&secret_store_key(SECRET_KIND_INTEGRATION, "huggingface")),
            Some("hf_live_test_secret".to_string())
        );
    }

    #[test]
    fn test_t0_03_load_migrates_legacy_plaintext_secret_config() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("config.json");
        let legacy = r#"{
  "network": "testnet",
  "chain_id": 40204,
  "data_dir": "/tmp/test",
  "rpc_port": 18545,
  "p2p_port": 30304,
  "mcp_port": 9600,
  "bootnodes": [],
  "theme": "dark",
  "ai_keys": { "anthropic": "sk-ant-legacy-secret" },
  "ai_priority": ["local", "anthropic"],
  "logseq_graph_path": "/tmp/logseq",
  "logseq_enabled": false,
  "auto_journal": false,
  "on_chain_anchoring": false,
  "integration_tokens": { "github": "ghp_legacy_secret" }
}"#;
        std::fs::write(&path, legacy).expect("write legacy config");
        let store = TestSecretStore::default();

        let migrated = AppConfig::load_from_path_with_secret_store(&path, &store);
        let raw = std::fs::read_to_string(&path).expect("read migrated config");

        assert!(!raw.contains("sk-ant-legacy-secret"));
        assert!(!raw.contains("ghp_legacy_secret"));
        assert!(migrated
            .ai_keys
            .get("anthropic")
            .is_some_and(|value| is_secret_marker(value)));
        assert!(migrated
            .integration_tokens
            .get("github")
            .is_some_and(|value| is_secret_marker(value)));
        assert_eq!(
            migrated.get_ai_key_with_secret_store("anthropic", &store),
            Some("sk-ant-legacy-secret".to_string())
        );
        assert_eq!(
            migrated.get_integration_token_with_secret_store("github", &store),
            Some("ghp_legacy_secret".to_string())
        );
    }

    #[test]
    fn test_t0_03_config_debug_redacts_secret_values() {
        let mut config = AppConfig::default();
        config
            .ai_keys
            .insert("openai".to_string(), "sk-debug-secret".to_string());
        config
            .integration_tokens
            .insert("github".to_string(), "ghp_debug_secret".to_string());

        let debug = format!("{:?}", config);

        assert!(!debug.contains("sk-debug-secret"));
        assert!(!debug.contains("ghp_debug_secret"));
        assert!(debug.contains("openai"));
        assert!(debug.contains("github"));
    }

    #[test]
    fn test_t0_03_config_save_fails_closed_if_secret_store_rejects() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("config.json");
        let store = TestSecretStore::failing();
        let mut config = AppConfig::default();
        config
            .ai_keys
            .insert("openai".to_string(), "sk-never-write-plaintext".to_string());

        let result = config.save_to_path_with_secret_store(&path, &store);

        assert!(result.is_err());
        assert!(
            !path.exists(),
            "config save must fail before writing plaintext secrets"
        );
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
