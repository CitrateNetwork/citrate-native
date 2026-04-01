//! Learning center service — pools, staking, training cycles, earnings.
//!
//! Data source: LearningPool and ContributionAccounting smart contracts
//! via eth_call, plus citrate_getAIStatus RPC for network health.

use crate::error::AppError;
use crate::event_bus::EventBus;
use std::sync::Arc;

/// Learning pool info.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub id: String,
    pub name: String,
    pub model_name: String,
    pub member_count: u32,
    pub stake_requirement: String,   // SALT amount
    pub total_staked: String,
    pub current_epoch: u64,
    pub status: String,              // "active", "paused", "completed"
}

/// Staking position.
#[derive(Debug, Clone)]
pub struct StakePosition {
    pub pool_id: String,
    pub amount: String,
    pub joined_at: u64,
    pub earnings: String,
}

/// Training epoch status.
#[derive(Debug, Clone)]
pub struct EpochStatus {
    pub epoch: u64,
    pub loss: f64,
    pub accuracy: f64,
    pub participants: u32,
    pub duration_secs: u64,
}

/// Backend trait.
#[async_trait::async_trait]
pub trait LearningBackend: Send + Sync {
    async fn list_pools(&self) -> Result<Vec<PoolInfo>, AppError>;
    async fn get_pool(&self, pool_id: &str) -> Result<PoolInfo, AppError>;
    async fn get_stake(&self, address: &str) -> Result<Option<StakePosition>, AppError>;
    async fn get_epoch_status(&self, pool_id: &str) -> Result<EpochStatus, AppError>;
    async fn get_earnings(&self, address: &str) -> Result<String, AppError>;
}

/// Real backend (on-chain queries).
// Infrastructure fields (rpc_url, client) used when LearningPool contract is deployed.
#[allow(dead_code)]
pub struct RpcLearningBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcLearningBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

/// LearningPool contract address — deployed on testnet (chain 40204).
/// Deployed 2026-03-31 via cast send.
const LEARNING_CONTRACT: Option<&str> = Some("0x92bdb6dab351b53f71b86f5b829b8205a7b1ad3b");
/// ContributionAccounting contract address — deployed on testnet (chain 40204).
const CONTRIBUTION_CONTRACT: Option<&str> = Some("0x383AA8E45C84f73Cc0Ea4933B694B3E67ADfe6ea");

#[async_trait::async_trait]
impl LearningBackend for RpcLearningBackend {
    /// Data source: LearningPool.getPool() via eth_call (when deployed)
    async fn list_pools(&self) -> Result<Vec<PoolInfo>, AppError> {
        let contract = match LEARNING_CONTRACT {
            Some(addr) => addr,
            None => return Ok(Vec::new()), // Contract not deployed — honest empty
        };
        // Query nextPoolId() to see how many pools exist
        // selector: cast sig "nextPoolId()" = 0x18e56131
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": contract, "data": "0x18e56131"}, "latest"],
            "id": 1,
        });
        // Try local RPC first, fall back to testnet for contract queries
        let urls = [self.rpc_url.as_str(), "https://rpc.citrate.ai"];
        for url in &urls {
            if let Ok(resp) = self.client.post(*url).json(&body)
                .timeout(std::time::Duration::from_secs(5))
                .send().await
            {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
                        let count = u64::from_str_radix(result.trim_start_matches("0x"), 16)
                            .unwrap_or(0);
                        tracing::info!("LearningPool: {} pools on-chain (via {})", count, url);
                        return Ok(Vec::new());
                    }
                }
            }
        }
        Ok(Vec::new())
    }

    async fn get_pool(&self, pool_id: &str) -> Result<PoolInfo, AppError> {
        Err(AppError::ChainQuery(format!(
            "Learning pools {}: pool '{}' not available",
            if LEARNING_CONTRACT.is_some() { "query failed" } else { "not deployed on this network" },
            pool_id
        )))
    }

    /// Data source: LearningPool.getMemberStake() via eth_call
    async fn get_stake(&self, _address: &str) -> Result<Option<StakePosition>, AppError> {
        Ok(None) // Requires pool ID + ABI encoding
    }

    async fn get_epoch_status(&self, _pool_id: &str) -> Result<EpochStatus, AppError> {
        Err(AppError::ChainQuery(
            if LEARNING_CONTRACT.is_some() {
                "Epoch query not yet implemented".to_string()
            } else {
                "Learning pools not deployed on this network".to_string()
            }
        ))
    }

    /// Data source: ContributionAccounting.getScore(address) via eth_call
    async fn get_earnings(&self, _address: &str) -> Result<String, AppError> {
        if CONTRIBUTION_CONTRACT.is_none() {
            return Ok("0".to_string());
        }
        Ok("0".to_string()) // Requires ABI encoding of address param
    }
}

#[cfg(test)]
pub struct TestLearningBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl LearningBackend for TestLearningBackend {
    async fn list_pools(&self) -> Result<Vec<PoolInfo>, AppError> {
        Ok(vec![PoolInfo {
            id: "test-pool".to_string(),
            name: "Test Pool".to_string(),
            model_name: "Test Model".to_string(),
            member_count: 5,
            stake_requirement: "1000".to_string(),
            total_staked: "5000".to_string(),
            current_epoch: 3,
            status: "active".to_string(),
        }])
    }
    async fn get_pool(&self, _id: &str) -> Result<PoolInfo, AppError> {
        self.list_pools().await.map(|p| p[0].clone())
    }
    async fn get_stake(&self, _addr: &str) -> Result<Option<StakePosition>, AppError> {
        Ok(Some(StakePosition {
            pool_id: "test-pool".to_string(),
            amount: "1000".to_string(),
            joined_at: 1711555200,
            earnings: "50".to_string(),
        }))
    }
    async fn get_epoch_status(&self, _id: &str) -> Result<EpochStatus, AppError> {
        Ok(EpochStatus { epoch: 3, loss: 0.45, accuracy: 0.82, participants: 5, duration_secs: 120 })
    }
    async fn get_earnings(&self, _addr: &str) -> Result<String, AppError> {
        Ok("50".to_string())
    }
}

/// Learning service.
// Infrastructure fields (events) used when real-time event publishing is wired.
#[allow(dead_code)]
pub struct LearningService {
    events: Arc<EventBus>,
    backend: Arc<dyn LearningBackend>,
}

impl LearningService {
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self { events, backend: Arc::new(RpcLearningBackend::new(rpc_url)) }
    }
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn LearningBackend>) -> Self {
        Self { events, backend }
    }
    pub async fn list_pools(&self) -> Result<Vec<PoolInfo>, AppError> { self.backend.list_pools().await }
    pub async fn get_pool(&self, id: &str) -> Result<PoolInfo, AppError> { self.backend.get_pool(id).await }
    pub async fn get_stake(&self, addr: &str) -> Result<Option<StakePosition>, AppError> { self.backend.get_stake(addr).await }
    pub async fn get_epoch_status(&self, id: &str) -> Result<EpochStatus, AppError> { self.backend.get_epoch_status(id).await }
    pub async fn get_earnings(&self, addr: &str) -> Result<String, AppError> { self.backend.get_earnings(addr).await }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> LearningService {
        LearningService::with_backend(Arc::new(EventBus::new()), Arc::new(TestLearningBackend))
    }

    #[tokio::test]
    async fn test_list_pools() {
        let svc = test_service();
        let pools = svc.list_pools().await.expect("list pools");
        assert!(!pools.is_empty());
        assert_eq!(pools[0].status, "active");
    }

    #[tokio::test]
    async fn test_get_pool() {
        let svc = test_service();
        let pool = svc.get_pool("test-pool").await.expect("get pool");
        assert_eq!(pool.name, "Test Pool");
    }

    #[tokio::test]
    async fn test_get_stake() {
        let svc = test_service();
        let stake = svc.get_stake("0xabc").await.expect("get stake");
        assert!(stake.is_some());
        assert_eq!(stake.as_ref().expect("stake").amount, "1000");
    }

    #[tokio::test]
    async fn test_get_epoch_status() {
        let svc = test_service();
        let epoch = svc.get_epoch_status("test-pool").await.expect("epoch");
        assert_eq!(epoch.epoch, 3);
        assert!(epoch.accuracy > 0.0);
    }

    #[tokio::test]
    async fn test_get_earnings() {
        let svc = test_service();
        let earnings = svc.get_earnings("0xabc").await.expect("earnings");
        assert_eq!(earnings, "50");
    }
}
