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
    async fn run_inference(&self, model_id: &str, input: &str) -> Result<InferenceResult, AppError>;
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
                let json: serde_json::Value = response.json().await
                    .map_err(|e| AppError::Network(format!("Model list parse failed: {}", e)))?;

                if let Some(models) = json.pointer("/result/models").and_then(|m| m.as_array()) {
                    Ok(models.iter().filter_map(|m| {
                        Some(ModelInfo {
                            id: m.get("id")?.as_str()?.to_string(),
                            name: m.get("name")?.as_str()?.to_string(),
                            version: m.get("version").and_then(|v| v.as_str()).unwrap_or("1.0").to_string(),
                            description: m.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                            framework: m.get("framework").and_then(|f| f.as_str()).unwrap_or("gguf").to_string(),
                            size_bytes: m.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
                            total_inferences: m.get("total_inferences").and_then(|t| t.as_u64()).unwrap_or(0),
                            success_rate: m.get("success_rate").and_then(|s| s.as_f64()).unwrap_or(0.0),
                            owner: m.get("owner").and_then(|o| o.as_str()).unwrap_or("").to_string(),
                        })
                    }).collect())
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
        models.into_iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| AppError::ModelNotLoaded(model_id.to_string()))
    }

    async fn run_inference(&self, model_id: &str, input: &str) -> Result<InferenceResult, AppError> {
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

        let response = self.client.post(&self.rpc_url)
            .json(&body).send().await
            .map_err(|e| AppError::Network(format!("Inference RPC failed: {}", e)))?;

        let json: serde_json::Value = response.json().await
            .map_err(|e| AppError::Network(format!("Inference parse failed: {}", e)))?;

        let content = json.pointer("/result/choices/0/message/content")
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

/// Model service.
// Infrastructure fields (events) used when real-time event publishing is wired.
#[allow(dead_code)]
pub struct ModelService {
    events: Arc<EventBus>,
    backend: Arc<dyn ModelBackend>,
    cached_models: Arc<RwLock<Vec<ModelInfo>>>,
}

impl ModelService {
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self {
            events,
            backend: Arc::new(RpcModelBackend::new(rpc_url)),
            cached_models: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn ModelBackend>) -> Self {
        Self {
            events,
            backend,
            cached_models: Arc::new(RwLock::new(Vec::new())),
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

    pub async fn run_inference(&self, model_id: &str, input: &str) -> Result<InferenceResult, AppError> {
        self.backend.run_inference(model_id, input).await
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
        let result = svc.run_inference("test-model", "hello").await.expect("inference");
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
}
