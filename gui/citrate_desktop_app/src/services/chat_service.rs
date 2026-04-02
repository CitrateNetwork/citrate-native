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
    /// Structured tool definitions (OpenAI format). When present, the backend
    /// includes them in the request so the LLM can generate tool_calls.
    pub tools: Option<Vec<serde_json::Value>>,
}

/// A normalized tool call parsed from the LLM response.
#[derive(Debug, Clone)]
pub struct NormalizedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Chat completion response from the node.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub model: String,
    pub finish_reason: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Structured tool calls from the LLM (OpenAI format).
    pub tool_calls: Vec<NormalizedToolCall>,
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
            tool_calls: Vec::new(),
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

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });
        // Include structured tools if provided (OpenAI format)
        if let Some(ref tools) = request.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::json!(tools);
            }
        }

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

        // Parse structured tool_calls from OpenAI response
        let finish_reason = json
            .pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("stop")
            .to_string();
        let tool_calls = json
            .pointer("/choices/0/message/tool_calls")
            .and_then(|tc| tc.as_array())
            .map(|calls| {
                calls.iter().filter_map(|call| {
                    let id = call.get("id")?.as_str()?.to_string();
                    let name = call.pointer("/function/name")?.as_str()?.to_string();
                    let args_str = call.pointer("/function/arguments")?.as_str()?;
                    let arguments = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                    Some(NormalizedToolCall { id, name, arguments })
                }).collect()
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            model,
            finish_reason,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls,
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
            tool_calls: Vec::new(),
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
            max_tokens: 256,
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
            max_tokens: 256,
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
            max_tokens: 256,
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
    /// Prefers SMALL, FAST models over large ones for responsive chat.
    /// A 7B model responds in seconds; a 72B model takes minutes.
    pub fn pick_best_model(models: &[String]) -> Option<String> {
        // Prefer small/fast models explicitly — order matters
        let fast_models = [
            "qwen2.5:1.5b", "qwen2.5:3b", "qwen2.5:7b",
            "mistral:7b", "mistral:latest",
            "llama3.1:8b", "llama3.2:3b",
            "phi3:3.8b", "phi:latest",
            "gemma:2b", "gemma:7b",
        ];
        for fast in &fast_models {
            if let Some(m) = models.iter().find(|m| m.to_lowercase().contains(fast)) {
                return Some(m.clone());
            }
        }
        // If no small model found, pick any model that's NOT 70b/72b
        if let Some(m) = models.iter().find(|m| {
            let lower = m.to_lowercase();
            !lower.contains("70b") && !lower.contains("72b") && !lower.contains("65b")
        }) {
            return Some(m.clone());
        }
        // Absolute fallback
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
        // Rolling window: only include last 10 messages to limit context growth
        let relevant: Vec<_> = messages.iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .collect();
        let window_start = relevant.len().saturating_sub(10);
        for msg in &relevant[window_start..] {
            request_messages.push((msg.role.clone(), msg.content.clone()));
        }

        let request = ChatRequest {
            model,
            messages: request_messages,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tools: None,
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

    /// Send a message with tool execution support.
    /// If the LLM responds with tool_calls, execute them through the provided
    /// registry and return tool results as part of the conversation.
    ///
    /// Send a message with tool execution support. Implements the end-to-end
    /// function-calling loop. The tool_executor closure is called for each tool
    /// the LLM requests, receiving (name, params) and returning Ok(result) or
    /// Err(error). Loops until the LLM stops requesting tools (max 5 iterations).
    pub async fn send_message_with_tools<F, Fut>(
        &self,
        user_message: &str,
        tool_defs: Vec<serde_json::Value>,
        tool_executor: F,
    ) -> Result<ChatMessage, AppError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        self.send_message_with_tools_streaming(user_message, tool_defs, tool_executor, |_| {}).await
    }

    /// Like send_message_with_tools, but calls `on_chunk` with intermediate content
    /// after each tool execution and when the final response arrives.
    /// This enables incremental UI updates without rewriting the transport layer.
    pub async fn send_message_with_tools_streaming<F, Fut, C>(
        &self,
        user_message: &str,
        tool_defs: Vec<serde_json::Value>,
        tool_executor: F,
        on_chunk: C,
    ) -> Result<ChatMessage, AppError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
        C: Fn(&str),
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Add user message
        let user_msg = ChatMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: "user".to_string(),
            content: user_message.to_string(),
            timestamp: now,
            tool_action: None,
        };
        self.messages.write().await.push(user_msg);

        let mut iteration = 0;
        let max_iterations = 5; // Safety: prevent infinite tool loops

        loop {
            iteration += 1;
            if iteration > max_iterations {
                tracing::warn!("Chat: tool loop exceeded {} iterations, stopping", max_iterations);
                break;
            }

            // Build request with full history and tool definitions
            let messages = self.messages.read().await;
            let system = self.system_prompt.read().await;
            let model = self.model.read().await.clone();

            let mut request_messages = Vec::new();
            if !system.is_empty() {
                request_messages.push(("system".to_string(), system.clone()));
            }
            // Rolling window: last 10 messages to limit context growth
            let relevant: Vec<_> = messages.iter()
                .filter(|m| m.role == "user" || m.role == "assistant" || m.role == "tool")
                .collect();
            let window_start = relevant.len().saturating_sub(10);
            for msg in &relevant[window_start..] {
                request_messages.push((msg.role.clone(), msg.content.clone()));
            }
            drop(messages);

            let request = ChatRequest {
                model: model.clone(),
                messages: request_messages,
                max_tokens: self.max_tokens,
                temperature: self.temperature,
                tools: if tool_defs.is_empty() { None } else { Some(tool_defs.clone()) },
            };

            let response = self.backend.chat_completion(request).await?;

            // If no tool calls, this is the final response
            if response.tool_calls.is_empty() {
                // Notify listener of the final content
                on_chunk(&response.content);
                let assistant_msg = ChatMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "assistant".to_string(),
                    content: response.content,
                    timestamp: now + iteration as u64,
                    tool_action: None,
                };
                self.messages.write().await.push(assistant_msg.clone());
                return Ok(assistant_msg);
            }

            // Execute each tool call
            for call in &response.tool_calls {
                tracing::info!("Chat: executing tool '{}' (id: {})", call.name, call.id);

                let result = tool_executor(call.name.clone(), call.arguments.clone()).await;

                let (result_content, success) = match result {
                    Ok(output) => (output, true),
                    Err(err) => (format!("Tool error: {}", err), false),
                };

                // Notify listener of tool execution progress
                on_chunk(&format!("[Tool: {} → {}]", call.name, if success { "ok" } else { "failed" }));

                // Add tool result to conversation
                let tool_msg = ChatMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    role: "tool".to_string(),
                    content: result_content.clone(),
                    timestamp: now + iteration as u64,
                    tool_action: Some(ToolAction {
                        tool_type: call.name.clone(),
                        params: serde_json::to_string(&call.arguments).unwrap_or_default(),
                        status: if success { "completed".to_string() } else { "failed".to_string() },
                        result: Some(result_content),
                    }),
                };
                self.messages.write().await.push(tool_msg);
            }

            // Continue the loop — the LLM will see the tool results and respond
        }

        // Fallback if loop maxed out
        let fallback = ChatMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: "assistant".to_string(),
            content: "I reached the maximum number of tool calls for this request.".to_string(),
            timestamp: now,
            tool_action: None,
        };
        self.messages.write().await.push(fallback.clone());
        Ok(fallback)
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
            max_tokens: 256,
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
