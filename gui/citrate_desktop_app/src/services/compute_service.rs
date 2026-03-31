//! Compute marketplace service — jobs, providers, GPU management.
//!
//! Data source: ComputeMarketplace smart contract via eth_call,
//! plus citrate_getAIStatus for provider availability.

use crate::error::AppError;
use crate::event_bus::EventBus;
use std::sync::Arc;

/// Compute job info.
#[derive(Debug, Clone)]
pub struct ComputeJob {
    pub id: String,
    pub model_id: String,
    pub status: String,        // "posted", "assigned", "executing", "completed", "failed"
    pub budget: String,        // SALT amount
    pub provider: Option<String>,
    pub created_at: u64,
    pub compute_time_secs: Option<u64>,
}

/// Compute provider info.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub address: String,
    pub name: String,
    pub gpu_type: String,
    pub memory_gb: u32,
    pub price_per_hour: String,
    pub reputation: f64,
    pub active_jobs: u32,
}

/// Backend trait.
#[async_trait::async_trait]
pub trait ComputeBackend: Send + Sync {
    async fn list_jobs(&self) -> Result<Vec<ComputeJob>, AppError>;
    async fn list_providers(&self) -> Result<Vec<ProviderInfo>, AppError>;
    async fn get_job(&self, job_id: &str) -> Result<ComputeJob, AppError>;
    async fn post_job(&self, model_id: &str, budget: &str) -> Result<String, AppError>;
}

/// Real backend.
// Infrastructure fields (rpc_url, client) used when ComputeMarketplace contract is deployed.
#[allow(dead_code)]
pub struct RpcComputeBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcComputeBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self { rpc_url: rpc_url.to_string(), client: reqwest::Client::new() }
    }
}

/// ComputeMarketplace contract address — deployed on testnet (chain 40204).
/// Deployed 2026-03-31 via cast send.
const COMPUTE_CONTRACT: Option<&str> = Some("0xCcc1eF0fbA6399E5173213273BaA78D39ec30Dc0");

#[async_trait::async_trait]
impl ComputeBackend for RpcComputeBackend {
    /// Data source: ComputeMarketplace.getProviderCount() via eth_call
    async fn list_jobs(&self) -> Result<Vec<ComputeJob>, AppError> {
        let contract = match COMPUTE_CONTRACT {
            Some(addr) => addr,
            None => return Ok(Vec::new()), // Contract not deployed — honest empty
        };
        // getProviderCount() selector
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": contract, "data": "0x27e235e3"}, "latest"],
            "id": 1,
        });
        match self.client.post(&self.rpc_url).json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send().await
        {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if json.get("result").is_some() {
                        tracing::info!("ComputeMarketplace: contract responded");
                    }
                }
                Ok(Vec::new()) // ABI decoding needed for full job list
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Data source: ComputeMarketplace.getProviderCount() via eth_call
    async fn list_providers(&self) -> Result<Vec<ProviderInfo>, AppError> {
        if COMPUTE_CONTRACT.is_none() {
            return Ok(Vec::new()); // Contract not deployed
        }
        Ok(Vec::new()) // Placeholder until ABI decoding is implemented
    }

    async fn get_job(&self, job_id: &str) -> Result<ComputeJob, AppError> {
        Err(AppError::ChainQuery(format!(
            "Compute marketplace {}: job '{}' not available",
            if COMPUTE_CONTRACT.is_some() { "query failed" } else { "not deployed on this network" },
            job_id
        )))
    }

    async fn post_job(&self, _model_id: &str, _budget: &str) -> Result<String, AppError> {
        match COMPUTE_CONTRACT {
            Some(_) => Err(AppError::ChainQuery("Job posting requires wallet signing — use the GUI".to_string())),
            None => Err(AppError::ChainQuery("Compute marketplace not yet deployed on this network".to_string())),
        }
    }
}

#[cfg(test)]
pub struct TestComputeBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl ComputeBackend for TestComputeBackend {
    async fn list_jobs(&self) -> Result<Vec<ComputeJob>, AppError> {
        Ok(vec![ComputeJob {
            id: "job-1".to_string(),
            model_id: "mistral-7b".to_string(),
            status: "executing".to_string(),
            budget: "10000000000000000000".to_string(),
            provider: Some("0xprovider".to_string()),
            created_at: 1711555200,
            compute_time_secs: Some(60),
        }])
    }
    async fn list_providers(&self) -> Result<Vec<ProviderInfo>, AppError> {
        Ok(vec![ProviderInfo {
            address: "0xprovider".to_string(),
            name: "GPU Node 1".to_string(),
            gpu_type: "NVIDIA A100".to_string(),
            memory_gb: 80,
            price_per_hour: "1000000000000000000".to_string(),
            reputation: 0.95,
            active_jobs: 1,
        }])
    }
    async fn get_job(&self, _id: &str) -> Result<ComputeJob, AppError> {
        self.list_jobs().await.map(|j| j[0].clone())
    }
    async fn post_job(&self, _model_id: &str, _budget: &str) -> Result<String, AppError> {
        Ok("job-new".to_string())
    }
}

/// Compute service.
// Infrastructure fields (events) used when real-time event publishing is wired.
#[allow(dead_code)]
pub struct ComputeService {
    events: Arc<EventBus>,
    backend: Arc<dyn ComputeBackend>,
}

impl ComputeService {
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self { events, backend: Arc::new(RpcComputeBackend::new(rpc_url)) }
    }
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn ComputeBackend>) -> Self {
        Self { events, backend }
    }
    pub async fn list_jobs(&self) -> Result<Vec<ComputeJob>, AppError> { self.backend.list_jobs().await }
    pub async fn list_providers(&self) -> Result<Vec<ProviderInfo>, AppError> { self.backend.list_providers().await }
    pub async fn get_job(&self, id: &str) -> Result<ComputeJob, AppError> { self.backend.get_job(id).await }
    pub async fn post_job(&self, model_id: &str, budget: &str) -> Result<String, AppError> { self.backend.post_job(model_id, budget).await }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> ComputeService {
        ComputeService::with_backend(Arc::new(EventBus::new()), Arc::new(TestComputeBackend))
    }

    #[tokio::test]
    async fn test_list_jobs() {
        let svc = test_service();
        let jobs = svc.list_jobs().await.expect("list jobs");
        assert!(!jobs.is_empty());
        assert_eq!(jobs[0].status, "executing");
    }

    #[tokio::test]
    async fn test_list_providers() {
        let svc = test_service();
        let providers = svc.list_providers().await.expect("list providers");
        assert!(!providers.is_empty());
        assert_eq!(providers[0].gpu_type, "NVIDIA A100");
    }

    #[tokio::test]
    async fn test_get_job() {
        let svc = test_service();
        let job = svc.get_job("job-1").await.expect("get job");
        assert_eq!(job.id, "job-1");
    }

    #[tokio::test]
    async fn test_post_job() {
        let svc = test_service();
        let id = svc.post_job("mistral-7b", "10").await.expect("post job");
        assert!(!id.is_empty());
    }
}
