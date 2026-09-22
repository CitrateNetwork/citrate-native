//! Model registry service — browse, deploy, and run inference on AI models.
//!
//! Data source: citrate_listModels, citrate_getModel, citrate_deployModel,
//! citrate_runInference JSON-RPC methods on the connected node.

use crate::error::AppError;
use crate::event_bus::EventBus;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Model info from the on-chain registry.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub framework: String,
    pub size_bytes: u64,
    pub total_inferences: u64,
    pub success_rate: f64,
    pub owner: String,
}

/// Inference result.
#[derive(Debug, Clone)]
pub struct InferenceResult {
    pub output: String,
    pub model_id: String,
    pub gas_used: u64,
    pub execution_time_ms: u64,
}

/// Backend trait for model operations.
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError>;
    async fn get_model(&self, model_id: &str) -> Result<ModelInfo, AppError>;
    async fn run_inference(&self, model_id: &str, input: &str)
        -> Result<InferenceResult, AppError>;
}

/// Real backend calling chain RPC.
pub struct RpcModelBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcModelBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ModelBackend for RpcModelBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        // F-04 fix: query RPC for real model list, don't return canned data
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "citrate_listModels",
            "params": [],
            "id": 1,
        });

        match self.client.post(&self.rpc_url).json(&body).send().await {
            Ok(response) => {
                let json: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|e| AppError::Network(format!("Model list parse failed: {}", e)))?;

                if let Some(models) = json.pointer("/result/models").and_then(|m| m.as_array()) {
                    Ok(models
                        .iter()
                        .filter_map(|m| {
                            Some(ModelInfo {
                                id: m.get("id")?.as_str()?.to_string(),
                                name: m.get("name")?.as_str()?.to_string(),
                                version: m
                                    .get("version")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("1.0")
                                    .to_string(),
                                description: m
                                    .get("description")
                                    .and_then(|d| d.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                framework: m
                                    .get("framework")
                                    .and_then(|f| f.as_str())
                                    .unwrap_or("gguf")
                                    .to_string(),
                                size_bytes: m
                                    .get("size_bytes")
                                    .and_then(|s| s.as_u64())
                                    .unwrap_or(0),
                                total_inferences: m
                                    .get("total_inferences")
                                    .and_then(|t| t.as_u64())
                                    .unwrap_or(0),
                                success_rate: m
                                    .get("success_rate")
                                    .and_then(|s| s.as_f64())
                                    .unwrap_or(0.0),
                                owner: m
                                    .get("owner")
                                    .and_then(|o| o.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            })
                        })
                        .collect())
                } else {
                    // RPC returned but no models — registry may not be deployed
                    Ok(vec![])
                }
            }
            Err(_) => {
                // RPC unavailable — return empty list, don't fake models
                // F-04: truthful "no models available" state
                Ok(vec![])
            }
        }
    }

    async fn get_model(&self, model_id: &str) -> Result<ModelInfo, AppError> {
        let models = self.list_models().await?;
        models
            .into_iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| AppError::ModelNotLoaded(model_id.to_string()))
    }

    async fn run_inference(
        &self,
        model_id: &str,
        input: &str,
    ) -> Result<InferenceResult, AppError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "citrate_chatCompletion",
            "params": [{
                "model": model_id,
                "messages": [{"role": "user", "content": input}],
                "max_tokens": 256,
                "temperature": 0.7,
            }],
            "id": 1,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Inference RPC failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Inference parse failed: {}", e)))?;

        let content = json
            .pointer("/result/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("No output")
            .to_string();

        Ok(InferenceResult {
            output: content,
            model_id: model_id.to_string(),
            gas_used: 0,
            execution_time_ms: 0,
        })
    }
}

#[cfg(test)]
pub struct TestModelBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl ModelBackend for TestModelBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        Ok(vec![ModelInfo {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            version: "1.0".to_string(),
            description: "For testing".to_string(),
            framework: "test".to_string(),
            size_bytes: 100,
            total_inferences: 5,
            success_rate: 0.95,
            owner: "test".to_string(),
        }])
    }
    async fn get_model(&self, _id: &str) -> Result<ModelInfo, AppError> {
        self.list_models().await.map(|m| m[0].clone())
    }
    async fn run_inference(&self, _id: &str, _input: &str) -> Result<InferenceResult, AppError> {
        Ok(InferenceResult {
            output: "Test inference output".to_string(),
            model_id: "test-model".to_string(),
            gas_used: 21000,
            execution_time_ms: 100,
        })
    }
}

// ============================================================================
// Publish state machine — tracks model lifecycle from local to verified
// ============================================================================

/// Canonical publish states. Only advances when real evidence exists.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelPublishState {
    Local,
    ArtifactHashed,
    Pinned,
    Submitted,
    Confirmed,
    ReadbackVerified,
    Ready,
    Failed,
}

impl ModelPublishState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ArtifactHashed => "hashed",
            Self::Pinned => "pinned",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
            Self::ReadbackVerified => "verified",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Identity of a model artifact, derived from its content.
#[derive(Debug, Clone)]
pub struct ModelArtifactIdentity {
    pub file_name: String,
    pub file_size_bytes: u64,
    pub content_hash_keccak256: String,
    pub cid: Option<String>,
}

/// Full publish record tracking the lifecycle of a model deployment.
#[derive(Debug, Clone)]
pub struct ModelPublishRecord {
    pub local_path: String,
    pub model_id: String,
    pub owner: String,
    pub state: ModelPublishState,
    pub artifact: ModelArtifactIdentity,
    pub tx_hash: Option<String>,
    pub receipt_block: Option<u64>,
    pub receipt_status: Option<bool>,
    pub readback_owner: Option<String>,
    pub readback_cid: Option<String>,
    pub last_error: Option<String>,
}

/// Model service.
pub struct ModelService {
    #[allow(dead_code)] // Events bus will be used for publish lifecycle events
    events: Arc<EventBus>,
    backend: Arc<dyn ModelBackend>,
    cached_models: Arc<RwLock<Vec<ModelInfo>>>,
    rpc_url: String,
    client: reqwest::Client,
    /// Current publish record (if a publish is in progress)
    publish_record: Arc<RwLock<Option<ModelPublishRecord>>>,
}

impl ModelService {
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self {
            events,
            backend: Arc::new(RpcModelBackend::new(rpc_url)),
            cached_models: Arc::new(RwLock::new(Vec::new())),
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
            publish_record: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn ModelBackend>) -> Self {
        Self {
            events,
            backend,
            cached_models: Arc::new(RwLock::new(Vec::new())),
            rpc_url: String::new(),
            client: reqwest::Client::new(),
            publish_record: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn refresh_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let models = self.backend.list_models().await?;
        *self.cached_models.write().await = models.clone();
        Ok(models)
    }

    pub async fn get_models(&self) -> Vec<ModelInfo> {
        self.cached_models.read().await.clone()
    }

    pub async fn get_model(&self, model_id: &str) -> Result<ModelInfo, AppError> {
        self.backend.get_model(model_id).await
    }

    pub async fn run_inference(
        &self,
        model_id: &str,
        input: &str,
    ) -> Result<InferenceResult, AppError> {
        self.backend.run_inference(model_id, input).await
    }

    // ── Publish lifecycle methods ──

    /// Initialize a publish record from a local artifact.
    pub async fn init_publish(&self, path: &str, content_hash: &str, file_size: u64, owner: &str) {
        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let id_len = content_hash.len().min(16);
        let model_id = format!("0x{}", &content_hash[..id_len]);
        *self.publish_record.write().await = Some(ModelPublishRecord {
            local_path: path.to_string(),
            model_id,
            owner: owner.to_string(),
            state: ModelPublishState::ArtifactHashed,
            artifact: ModelArtifactIdentity {
                file_name,
                file_size_bytes: file_size,
                content_hash_keccak256: content_hash.to_string(),
                cid: None,
            },
            tx_hash: None,
            receipt_block: None,
            receipt_status: None,
            readback_owner: None,
            readback_cid: None,
            last_error: None,
        });
    }

    /// Advance state to Pinned with a verified CID.
    pub async fn mark_pinned(&self, cid: &str) {
        if let Some(ref mut record) = *self.publish_record.write().await {
            record.artifact.cid = Some(cid.to_string());
            record.state = ModelPublishState::Pinned;
        }
    }

    /// Advance state to Submitted with tx hash.
    pub async fn mark_submitted(&self, tx_hash: &str) {
        if let Some(ref mut record) = *self.publish_record.write().await {
            record.tx_hash = Some(tx_hash.to_string());
            record.state = ModelPublishState::Submitted;
        }
    }

    /// Poll for tx receipt. Returns the publish state after polling.
    /// Data source: eth_getTransactionReceipt via RPC.
    pub async fn poll_receipt(&self) -> Result<ModelPublishState, AppError> {
        let record = self.publish_record.read().await;
        let tx_hash = match record.as_ref().and_then(|r| r.tx_hash.as_ref()) {
            Some(h) => h.clone(),
            None => return Err(AppError::Config("No tx hash to poll".into())),
        };
        drop(record);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
            "id": 1,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Receipt poll failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Receipt parse failed: {}", e)))?;

        if json.get("result").is_some() && !json["result"].is_null() {
            let status = json["result"]["status"]
                .as_str()
                .map(|s| s == "0x1")
                .unwrap_or(false);
            let block = json["result"]["blockNumber"]
                .as_str()
                .and_then(|b| u64::from_str_radix(b.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);

            let mut record = self.publish_record.write().await;
            if let Some(ref mut r) = *record {
                r.receipt_status = Some(status);
                r.receipt_block = Some(block);
                if status {
                    r.state = ModelPublishState::Confirmed;
                } else {
                    r.state = ModelPublishState::Failed;
                    r.last_error = Some("Transaction reverted".into());
                }
                return Ok(r.state.clone());
            }
        }
        // No receipt yet — still pending
        Ok(ModelPublishState::Submitted)
    }

    /// Verify registry readback matches expected state.
    /// Data source: citrate_getModel RPC.
    pub async fn verify_readback(&self) -> Result<ModelPublishState, AppError> {
        let record = self.publish_record.read().await;
        let (model_id, expected_owner, expected_cid) = match record.as_ref() {
            Some(r) => (
                r.model_id.clone(),
                r.owner.clone(),
                r.artifact.cid.clone().unwrap_or_default(),
            ),
            None => return Err(AppError::Config("No publish record".into())),
        };
        drop(record);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "citrate_getModel",
            "params": [model_id],
            "id": 1,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Readback failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Readback parse failed: {}", e)))?;

        let result = &json["result"];
        if result.is_null() {
            let mut record = self.publish_record.write().await;
            if let Some(ref mut r) = *record {
                r.state = ModelPublishState::Failed;
                r.last_error = Some("Model not found in registry".into());
            }
            return Ok(ModelPublishState::Failed);
        }

        let observed_owner = result["owner"].as_str().unwrap_or("").to_string();
        let observed_cid = result
            .get("cid")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let mut record = self.publish_record.write().await;
        if let Some(ref mut r) = *record {
            r.readback_owner = Some(observed_owner.clone());
            r.readback_cid = Some(observed_cid.clone());

            // Verify owner matches
            if !expected_owner.is_empty()
                && observed_owner.to_lowercase() != expected_owner.to_lowercase()
            {
                r.state = ModelPublishState::Failed;
                r.last_error = Some(format!(
                    "Owner mismatch: expected {} got {}",
                    expected_owner, observed_owner
                ));
                return Ok(ModelPublishState::Failed);
            }

            // Verify CID matches (if we have one)
            if !expected_cid.is_empty() && !observed_cid.is_empty() && observed_cid != expected_cid
            {
                r.state = ModelPublishState::Failed;
                r.last_error = Some(format!(
                    "CID mismatch: expected {} got {}",
                    expected_cid, observed_cid
                ));
                return Ok(ModelPublishState::Failed);
            }

            r.state = ModelPublishState::ReadbackVerified;
            Ok(ModelPublishState::ReadbackVerified)
        } else {
            Err(AppError::Config("Publish record disappeared".into()))
        }
    }

    /// Get current publish state.
    pub async fn publish_state(&self) -> Option<String> {
        self.publish_record
            .read()
            .await
            .as_ref()
            .map(|r| r.state.as_str().to_string())
    }

    /// Get current publish record (for UI display).
    pub async fn publish_record(&self) -> Option<ModelPublishRecord> {
        self.publish_record.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> ModelService {
        let events = Arc::new(EventBus::new());
        ModelService::with_backend(events, Arc::new(TestModelBackend))
    }

    #[tokio::test]
    async fn test_refresh_models() {
        let svc = test_service();
        let models = svc.refresh_models().await.expect("refresh");
        assert!(!models.is_empty());
    }

    #[tokio::test]
    async fn test_get_models_cached() {
        let svc = test_service();
        assert!(svc.get_models().await.is_empty()); // not yet refreshed
        svc.refresh_models().await.expect("refresh");
        assert!(!svc.get_models().await.is_empty());
    }

    #[tokio::test]
    async fn test_get_model() {
        let svc = test_service();
        let model = svc.get_model("test-model").await.expect("get model");
        assert_eq!(model.name, "Test Model");
    }

    #[tokio::test]
    async fn test_run_inference() {
        let svc = test_service();
        let result = svc
            .run_inference("test-model", "hello")
            .await
            .expect("inference");
        assert!(!result.output.is_empty());
        assert_eq!(result.model_id, "test-model");
    }

    #[tokio::test]
    async fn test_model_info_fields() {
        let svc = test_service();
        let models = svc.refresh_models().await.expect("refresh");
        let model = &models[0];
        assert!(!model.id.is_empty());
        assert!(!model.name.is_empty());
        assert!(model.size_bytes > 0);
    }

    // ── Publish state machine tests ──

    #[tokio::test]
    async fn test_publish_init_sets_hashed() {
        let svc = test_service();
        svc.init_publish("/tmp/model.gguf", "abcdef1234567890", 1024, "0xowner")
            .await;
        let state = svc.publish_state().await;
        assert_eq!(state, Some("hashed".to_string()));
    }

    #[tokio::test]
    async fn test_publish_mark_pinned() {
        let svc = test_service();
        svc.init_publish("/tmp/model.gguf", "abcdef1234567890", 1024, "0xowner")
            .await;
        svc.mark_pinned("QmTestCid123").await;
        let state = svc.publish_state().await;
        assert_eq!(state, Some("pinned".to_string()));
        let record = svc.publish_record().await.expect("record exists");
        assert_eq!(record.artifact.cid, Some("QmTestCid123".to_string()));
    }

    #[tokio::test]
    async fn test_publish_mark_submitted() {
        let svc = test_service();
        svc.init_publish("/tmp/model.gguf", "abcdef1234567890", 1024, "0xowner")
            .await;
        svc.mark_pinned("QmTestCid123").await;
        svc.mark_submitted("0xtxhash123").await;
        let state = svc.publish_state().await;
        assert_eq!(state, Some("submitted".to_string()));
        let record = svc.publish_record().await.expect("record exists");
        assert_eq!(record.tx_hash, Some("0xtxhash123".to_string()));
    }

    #[tokio::test]
    async fn test_publish_state_machine_order() {
        let svc = test_service();
        // Start: no record
        assert!(svc.publish_state().await.is_none());

        // Init
        svc.init_publish("/tmp/model.gguf", "aabbccdd", 2048, "0xowner")
            .await;
        assert_eq!(svc.publish_state().await, Some("hashed".to_string()));

        // Pin
        svc.mark_pinned("QmCid").await;
        assert_eq!(svc.publish_state().await, Some("pinned".to_string()));

        // Submit
        svc.mark_submitted("0xtx").await;
        assert_eq!(svc.publish_state().await, Some("submitted".to_string()));

        // Cannot verify without receipt (poll_receipt needs RPC, tested separately)
    }

    #[tokio::test]
    async fn test_publish_artifact_identity() {
        let svc = test_service();
        svc.init_publish("/tmp/qwen.gguf", "deadbeef12345678", 4096, "0xowner")
            .await;
        let record = svc.publish_record().await.expect("record");
        assert_eq!(record.artifact.file_name, "qwen.gguf");
        assert_eq!(record.artifact.file_size_bytes, 4096);
        assert_eq!(record.artifact.content_hash_keccak256, "deadbeef12345678");
        // model_id derived from hash, not filename
        assert!(record.model_id.starts_with("0x"));
        assert!(record.model_id.contains("deadbeef"));
    }

    #[tokio::test]
    async fn test_same_hash_same_identity() {
        let svc = test_service();
        svc.init_publish("/tmp/model_a.gguf", "same_hash_123", 1024, "0xowner")
            .await;
        let id_a = svc.publish_record().await.expect("a").model_id;

        svc.init_publish("/tmp/model_b.gguf", "same_hash_123", 1024, "0xowner")
            .await;
        let id_b = svc.publish_record().await.expect("b").model_id;

        assert_eq!(id_a, id_b, "same hash must produce same model_id");
    }

    #[tokio::test]
    async fn test_different_hash_different_identity() {
        let svc = test_service();
        svc.init_publish("/tmp/model.gguf", "hash_aaaa", 1024, "0xowner")
            .await;
        let id_a = svc.publish_record().await.expect("a").model_id;

        svc.init_publish("/tmp/model.gguf", "hash_bbbb", 1024, "0xowner")
            .await;
        let id_b = svc.publish_record().await.expect("b").model_id;

        assert_ne!(id_a, id_b, "different hash must produce different model_id");
    }
}
