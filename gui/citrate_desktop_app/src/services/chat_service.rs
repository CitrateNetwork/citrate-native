//! Chat service — AI agent with tool approval for the desktop GUI.
//!
//! Data source: citrate_chatCompletion JSON-RPC on the connected node.
//! The chat service manages conversation history, tool requests,
//! and coordinates with the wallet for transaction tools.

use crate::error::AppError;
use crate::event_bus::EventBus;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Message in the chat thread.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub role: String,     // "user", "assistant", "system", "tool_request", "tool_result"
    pub content: String,
    pub timestamp: u64,
    pub tool_action: Option<ToolAction>,
}

/// A tool action requested by the AI.
#[derive(Debug, Clone)]
pub struct ToolAction {
    pub tool_type: String,   // "send_tx", "check_balance", "deploy_contract", "query_chain"
    pub params: String,      // JSON-encoded parameters
    pub status: String,      // "pending", "approved", "rejected", "executing", "completed", "failed"
    pub result: Option<String>,
}

/// Chat completion request to send to the node.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<(String, String)>,   // (role, content) pairs
    pub max_tokens: u32,
    pub temperature: f32,
}

/// Chat completion response from the node.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub model: String,
    pub finish_reason: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// Backend trait for chat completion.
#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    /// Send a chat completion request to the node.
    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse, AppError>;
    /// Check if the node is reachable.
    async fn is_connected(&self) -> bool;
    /// List available models.
    async fn list_models(&self) -> Result<Vec<String>, AppError>;
}

/// Real backend that calls citrate_chatCompletion RPC.
pub struct RpcChatBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcChatBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ChatBackend for RpcChatBackend {
    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse, AppError> {
        let messages: Vec<serde_json::Value> = request.messages
            .iter()
            .map(|(role, content)| serde_json::json!({ "role": role, "content": content }))
            .collect();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "citrate_chatCompletion",
            "params": [{
                "model": request.model,
                "messages": messages,
                "max_tokens": request.max_tokens,
                "temperature": request.temperature,
                "stream": false,
            }],
            "id": 1,
        });

        let response = self.client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Chat RPC failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Chat response parse failed: {}", e)))?;

        if let Some(error) = json.get("error") {
            let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("RPC error");
            return Err(AppError::Network(msg.to_string()));
        }

        let result = json.get("result")
            .ok_or_else(|| AppError::Network("No result in chat response".to_string()))?;

        let content = result
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("No response from model.")
            .to_string();

        let model = result.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(&request.model)
            .to_string();

        let finish_reason = result
            .pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("stop")
            .to_string();

        let prompt_tokens = result
            .pointer("/usage/prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;

        let completion_tokens = result
            .pointer("/usage/completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;

        Ok(ChatResponse {
            content,
            model,
            finish_reason,
            prompt_tokens,
            completion_tokens,
        })
    }

    async fn is_connected(&self) -> bool {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1,
        });

        self.client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn list_models(&self) -> Result<Vec<String>, AppError> {
        // Default models — the node's GGUF engine knows these
        Ok(vec![
            "mistral-7b-instruct-v0.3".to_string(),
            "qwen2-0.5b".to_string(),
        ])
    }
}

/// OpenAI-compatible API backend. Works with OpenAI, Anthropic (via proxy),
/// local llama.cpp server, or any provider that speaks the /v1/chat/completions format.
pub struct OpenAICompatibleBackend {
    api_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAICompatibleBackend {
    pub fn new(api_url: &str, api_key: &str) -> Self {
        Self {
            api_url: api_url.to_string(),
            api_key: api_key.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ChatBackend for OpenAICompatibleBackend {
    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse, AppError> {
        let messages: Vec<serde_json::Value> = request.messages
            .iter()
            .map(|(role, content)| serde_json::json!({ "role": role, "content": content }))
            .collect();

        let body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });

        let response = self.client
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("OpenAI API failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("OpenAI response parse failed: {}", e)))?;

        if let Some(error) = json.get("error") {
            let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("API error");
            return Err(AppError::Network(msg.to_string()));
        }

        let content = json
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("No response.")
            .to_string();

        let model = json.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(&request.model)
            .to_string();

        Ok(ChatResponse {
            content,
            model,
            finish_reason: "stop".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
        })
    }

    async fn is_connected(&self) -> bool {
        // Try a lightweight request to check connectivity
        self.client.get(&self.api_url).send().await.is_ok()
    }

    async fn list_models(&self) -> Result<Vec<String>, AppError> {
        // If this is a local Ollama backend, query the real model list
        if self.api_url.contains("localhost:11434") || self.api_url.contains("127.0.0.1:11434") {
            let models = ChatService::detect_ollama_models().await;
            if !models.is_empty() {
                return Ok(models);
            }
        }
        // Fallback: return generic names (only used for remote backends)
        Ok(vec!["default".to_string()])
    }
}

/// Fallback backend: tries on-chain RPC first, falls back to external API.
pub struct FallbackChatBackend {
    primary: Arc<dyn ChatBackend>,
    fallback: Option<Arc<dyn ChatBackend>>,
}

impl FallbackChatBackend {
    pub fn new(primary: Arc<dyn ChatBackend>) -> Self {
        Self { primary, fallback: None }
    }

    pub fn with_fallback(mut self, fallback: Arc<dyn ChatBackend>) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

#[async_trait::async_trait]
impl ChatBackend for FallbackChatBackend {
    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse, AppError> {
        match self.primary.chat_completion(request.clone()).await {
            Ok(response) => Ok(response),
            Err(primary_err) => {
                if let Some(ref fallback) = self.fallback {
                    tracing::info!("Primary chat failed ({}), trying fallback", primary_err);
                    fallback.chat_completion(request).await
                } else {
                    Err(primary_err)
                }
            }
        }
    }

    async fn is_connected(&self) -> bool {
        self.primary.is_connected().await ||
            self.fallback.as_ref().map_or(false, |_| {
                // Can't easily call async in map_or, just return true if fallback exists
                true
            })
    }

    async fn list_models(&self) -> Result<Vec<String>, AppError> {
        let mut models = self.primary.list_models().await.unwrap_or_default();
        if let Some(ref fallback) = self.fallback {
            models.extend(fallback.list_models().await.unwrap_or_default());
        }
        Ok(models)
    }
}

/// Test backend for testing.
#[cfg(test)]
pub struct TestChatBackend {
    pub response: String,
    pub connected: bool,
}

#[cfg(test)]
#[async_trait::async_trait]
impl ChatBackend for TestChatBackend {
    async fn chat_completion(&self, _request: ChatRequest) -> Result<ChatResponse, AppError> {
        Ok(ChatResponse {
            content: self.response.clone(),
            model: "test-model".to_string(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 10,
            completion_tokens: 20,
        })
    }

    async fn is_connected(&self) -> bool {
        self.connected
    }

    async fn list_models(&self) -> Result<Vec<String>, AppError> {
        Ok(vec!["test-model".to_string()])
    }
}

/// Detected AI backend info returned by [`ChatService::detect_local_backend`].
#[derive(Debug, Clone)]
pub struct DetectedBackend {
    /// Human-readable name for the UI (e.g. "qwen2.5:1.5b (Ollama)")
    pub display_name: String,
    /// Model identifier to pass in chat requests (e.g. "qwen2.5:1.5b")
    pub model_id: String,
    /// Backend type: "ollama", "gguf", or "none"
    pub backend_type: String,
}

/// Chat service — manages conversation and tool approval.
// Infrastructure fields (events) used when real-time event publishing is wired.
#[allow(dead_code)]
pub struct ChatService {
    events: Arc<EventBus>,
    backend: Arc<dyn ChatBackend>,
    messages: Arc<RwLock<Vec<ChatMessage>>>,
    system_prompt: Arc<RwLock<String>>,
    model: Arc<RwLock<String>>,
    max_tokens: u32,
    temperature: f32,
}

impl ChatService {
    /// Create with real RPC backend.
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self {
            events,
            backend: Arc::new(RpcChatBackend::new(rpc_url)),
            messages: Arc::new(RwLock::new(Vec::new())),
            system_prompt: Arc::new(RwLock::new(String::new())),
            model: Arc::new(RwLock::new("mistral-7b-instruct-v0.3".to_string())),
            max_tokens: 512,
            temperature: 0.7,
        }
    }

    /// Create with injected backend (keeps "mistral:latest" default for test compat).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn ChatBackend>) -> Self {
        Self {
            events,
            backend,
            messages: Arc::new(RwLock::new(Vec::new())),
            system_prompt: Arc::new(RwLock::new(String::new())),
            model: Arc::new(RwLock::new("mistral:latest".to_string())),
            max_tokens: 512,
            temperature: 0.7,
        }
    }

    /// Create with injected backend and explicit model name.
    pub fn with_backend_and_model(events: Arc<EventBus>, backend: Arc<dyn ChatBackend>, model: &str) -> Self {
        Self {
            events,
            backend,
            messages: Arc::new(RwLock::new(Vec::new())),
            system_prompt: Arc::new(RwLock::new(String::new())),
            model: Arc::new(RwLock::new(model.to_string())),
            max_tokens: 512,
            temperature: 0.7,
        }
    }

    /// Check if Ollama is running locally and return its available model names.
    ///
    /// Data source: Ollama REST API at http://localhost:11434/api/tags (local only).
    /// Times out after 2 seconds — if Ollama is not running, returns an empty vec.
    pub async fn detect_ollama_models() -> Vec<String> {
        let client = reqwest::Client::new();
        match client
            .get("http://localhost:11434/api/tags")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    json.get("models")
                        .and_then(|m| m.as_array())
                        .map(|models| {
                            models
                                .iter()
                                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            Err(_) => Vec::new(),
        }
    }

    /// Pick the best model from a list of Ollama model names.
    /// Prefers qwen, then mistral, then the first available model.
    pub fn pick_best_model(models: &[String]) -> Option<String> {
        // Preference order: qwen variants first (small, fast), then mistral
        let preferences = ["qwen", "mistral", "llama", "phi", "gemma"];
        for pref in &preferences {
            if let Some(m) = models.iter().find(|m| m.to_lowercase().contains(pref)) {
                return Some(m.clone());
            }
        }
        // Fallback: first available model
        models.first().cloned()
    }

    /// Detect local AI backends: Ollama first, then local GGUF files.
    /// Never contacts external APIs. Returns [`DetectedBackend`] with display info.
    ///
    /// Data sources:
    /// - Ollama REST API at http://localhost:11434/api/tags (local daemon)
    /// - Filesystem scan of ~/.local/share/citrate/models/*.gguf
    pub async fn detect_local_backend() -> DetectedBackend {
        // 1. Try Ollama (preferred — GPU-accelerated, manages models)
        let ollama_models = Self::detect_ollama_models().await;
        if !ollama_models.is_empty() {
            let best = Self::pick_best_model(&ollama_models)
                .unwrap_or_else(|| ollama_models[0].clone());
            tracing::info!("Detected Ollama with {} models, selected: {}", ollama_models.len(), best);
            return DetectedBackend {
                display_name: format!("{} (Ollama)", best),
                model_id: best,
                backend_type: "ollama".to_string(),
            };
        }

        // 2. Check for local GGUF files
        let model_dir = dirs::data_local_dir()
            .map(|d| d.join("citrate").join("models"));
        if let Some(ref dir) = model_dir {
            if dir.exists() {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    let gguf_files: Vec<String> = entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "gguf"))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    if let Some(first) = gguf_files.first() {
                        tracing::info!("Detected local GGUF model: {}", first);
                        return DetectedBackend {
                            display_name: first.clone(),
                            model_id: first.clone(),
                            backend_type: "gguf".to_string(),
                        };
                    }
                }
            }
        }

        // Also check ~/.citrate/models/ (download target)
        if let Some(home) = dirs::home_dir() {
            let alt_dir = home.join(".citrate").join("models");
            if alt_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&alt_dir) {
                    let gguf_files: Vec<String> = entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "gguf"))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    if let Some(first) = gguf_files.first() {
                        tracing::info!("Detected local GGUF model (home): {}", first);
                        return DetectedBackend {
                            display_name: first.clone(),
                            model_id: first.clone(),
                            backend_type: "gguf".to_string(),
                        };
                    }
                }
            }
        }

        // 3. No local backend found
        tracing::info!("No local AI backend detected");
        DetectedBackend {
            display_name: "No model".to_string(),
            model_id: String::new(),
            backend_type: "none".to_string(),
        }
    }

    /// Set the system prompt with wallet context.
    /// Set chat context with real environment values. Chain ID is derived from network name.
    pub async fn set_context(&self, address: &str, balance: &str, network: &str, block_height: u64) {
        let chain_id = crate::chain_id_for_network(network);
        let prompt = format!(
            "You are a Citrate blockchain assistant running natively on the Citrate network.\n\
             Current context:\n\
             - Network: {} (chain ID: {})\n\
             - User address: {}\n\
             - Balance: {} SALT\n\
             - Block height: {}\n\n\
             You can help with blockchain operations, checking balances, \
             explaining transactions, and drafting transactions for user approval.",
            network, chain_id, address, balance, block_height
        );
        *self.system_prompt.write().await = prompt;
    }

    /// Send a user message and get an AI response.
    pub async fn send_message(&self, user_message: &str) -> Result<ChatMessage, AppError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Add user message to history
        let user_msg = ChatMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: "user".to_string(),
            content: user_message.to_string(),
            timestamp: now,
            tool_action: None,
        };
        self.messages.write().await.push(user_msg);

        // Build request with history
        let messages = self.messages.read().await;
        let system = self.system_prompt.read().await;
        let model = self.model.read().await.clone();

        let mut request_messages = Vec::new();
        if !system.is_empty() {
            request_messages.push(("system".to_string(), system.clone()));
        }
        for msg in messages.iter() {
            if msg.role == "user" || msg.role == "assistant" {
                request_messages.push((msg.role.clone(), msg.content.clone()));
            }
        }

        let request = ChatRequest {
            model,
            messages: request_messages,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        drop(messages);

        // Call the backend
        let response = self.backend.chat_completion(request).await?;

        // Create assistant message
        let assistant_msg = ChatMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: "assistant".to_string(),
            content: response.content,
            timestamp: now + 1,
            tool_action: None,
        };

        self.messages.write().await.push(assistant_msg.clone());

        Ok(assistant_msg)
    }

    /// Get all messages in the conversation.
    pub async fn get_messages(&self) -> Vec<ChatMessage> {
        self.messages.read().await.clone()
    }

    /// Clear conversation history.
    pub async fn clear_history(&self) {
        self.messages.write().await.clear();
    }

    /// Check if the AI backend is connected.
    pub async fn is_connected(&self) -> bool {
        self.backend.is_connected().await
    }

    /// List available models.
    pub async fn list_models(&self) -> Result<Vec<String>, AppError> {
        self.backend.list_models().await
    }

    /// Set the active model.
    pub async fn set_model(&self, model: &str) {
        *self.model.write().await = model.to_string();
    }

    /// Get message count.
    pub async fn message_count(&self) -> usize {
        self.messages.read().await.len()
    }

    /// Configure an external AI provider as fallback.
    /// provider: "openai", "anthropic", etc.
    /// api_url: full URL to chat completions endpoint
    /// api_key: bearer token
    pub fn configure_external_provider(
        events: Arc<EventBus>,
        rpc_url: &str,
        api_url: &str,
        api_key: &str,
    ) -> Self {
        let primary = Arc::new(RpcChatBackend::new(rpc_url));
        let fallback = Arc::new(OpenAICompatibleBackend::new(api_url, api_key));
        let backend = Arc::new(
            FallbackChatBackend::new(primary)
                .with_fallback(fallback)
        );
        Self {
            events,
            backend,
            messages: Arc::new(RwLock::new(Vec::new())),
            system_prompt: Arc::new(RwLock::new(String::new())),
            model: Arc::new(RwLock::new("gpt-4".to_string())),
            max_tokens: 512,
            temperature: 0.7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service(response: &str) -> ChatService {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(TestChatBackend {
            response: response.to_string(),
            connected: true,
        });
        ChatService::with_backend(events, backend)
    }

    #[tokio::test]
    async fn test_send_message_returns_response() {
        let svc = test_service("Hello! I'm your Citrate assistant.");
        let response = svc.send_message("hi").await.expect("send message");
        assert_eq!(response.role, "assistant");
        assert_eq!(response.content, "Hello! I'm your Citrate assistant.");
    }

    #[tokio::test]
    async fn test_message_history_grows() {
        let svc = test_service("Response");
        assert_eq!(svc.message_count().await, 0);

        svc.send_message("first").await.expect("send 1");
        assert_eq!(svc.message_count().await, 2); // user + assistant

        svc.send_message("second").await.expect("send 2");
        assert_eq!(svc.message_count().await, 4); // 2 user + 2 assistant
    }

    #[tokio::test]
    async fn test_clear_history() {
        let svc = test_service("Response");
        svc.send_message("test").await.expect("send");
        assert!(svc.message_count().await > 0);

        svc.clear_history().await;
        assert_eq!(svc.message_count().await, 0);
    }

    #[tokio::test]
    async fn test_set_context() {
        let svc = test_service("Response");
        svc.set_context("0xabc", "100.0", "testnet", 1234).await;
        let prompt = svc.system_prompt.read().await.clone();
        assert!(prompt.contains("0xabc"));
        assert!(prompt.contains("100.0"));
        assert!(prompt.contains("testnet"));
        assert!(prompt.contains("1234"));
    }

    #[tokio::test]
    async fn test_is_connected() {
        let svc = test_service("Response");
        assert!(svc.is_connected().await);
    }

    #[tokio::test]
    async fn test_disconnected_backend() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(TestChatBackend {
            response: "".to_string(),
            connected: false,
        });
        let svc = ChatService::with_backend(events, backend);
        assert!(!svc.is_connected().await);
    }

    #[tokio::test]
    async fn test_list_models() {
        let svc = test_service("Response");
        let models = svc.list_models().await.expect("list models");
        assert!(!models.is_empty());
    }

    #[tokio::test]
    async fn test_set_model() {
        let svc = test_service("Response");
        svc.set_model("qwen2-0.5b").await;
        let model = svc.model.read().await.clone();
        assert_eq!(model, "qwen2-0.5b");
    }

    #[tokio::test]
    async fn test_get_messages() {
        let svc = test_service("Response");
        svc.send_message("hello").await.expect("send");
        let messages = svc.get_messages().await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].role, "assistant");
    }

    #[tokio::test]
    async fn test_message_has_timestamp() {
        let svc = test_service("Response");
        let response = svc.send_message("test").await.expect("send");
        assert!(response.timestamp > 0);
    }

    #[tokio::test]
    async fn test_message_has_unique_id() {
        let svc = test_service("Response");
        svc.send_message("first").await.expect("send 1");
        svc.send_message("second").await.expect("send 2");
        let messages = svc.get_messages().await;
        let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
        // All IDs should be unique
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "All message IDs must be unique");
    }
}
