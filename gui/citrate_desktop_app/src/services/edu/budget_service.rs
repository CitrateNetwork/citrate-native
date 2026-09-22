//! Budget allocation service.
//!
//! Data source: BudgetAllocation (0x7125...3e) via eth_call on chain 40204.
//! Queries per-classroom budget status: allocated, remaining, spent, monthly limit.

use super::abi;
use crate::error::AppError;

/// Contract address on chain 40204 (deployed 2026-04-05).
const BUDGET_ADDRESS: &str = "0xAd5d57aD9bB17d34Debb88566ab2F5dB879Cc46F";

/// Per-classroom budget snapshot.
#[derive(Debug, Clone)]
pub struct BudgetInfo {
    pub classroom_id: u64,
    pub allocated: u64,
    pub remaining: u64,
    pub spent: u64,
    pub monthly_limit: u64,
}

/// Backend trait for budget queries.
#[async_trait::async_trait]
pub trait BudgetBackend: Send + Sync {
    /// Get the full budget snapshot for a classroom.
    async fn get_budget(&self, classroom_id: u64) -> Result<BudgetInfo, AppError>;

    /// Get remaining budget for a classroom.
    async fn get_remaining(&self, classroom_id: u64) -> Result<u64, AppError>;

    /// Get total spent for a classroom.
    async fn get_spent(&self, classroom_id: u64) -> Result<u64, AppError>;
}

/// RPC-backed implementation.
pub struct RpcBudgetBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcBudgetBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn eth_call(&self, data: &str) -> Result<String, AppError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": BUDGET_ADDRESS, "data": data}, "latest"],
            "id": 1,
        });

        let resp = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Budget RPC call failed: {}", e)))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Budget response parse failed: {}", e)))?;

        json.get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let err_msg = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                AppError::ContractCall {
                    contract: BUDGET_ADDRESS.to_string(),
                    method: data[..10.min(data.len())].to_string(),
                    reason: err_msg.to_string(),
                }
            })
    }
}

#[async_trait::async_trait]
impl BudgetBackend for RpcBudgetBackend {
    /// Data source: BudgetAllocation.getAllocated/getRemaining/getSpent/getMonthlyLimit(uint256)
    async fn get_budget(&self, classroom_id: u64) -> Result<BudgetInfo, AppError> {
        let allocated_data = abi::encode_call_uint256("getAllocated(uint256)", classroom_id);
        let allocated_hex = self.eth_call(&allocated_data).await?;
        let allocated = abi::decode_uint64(&allocated_hex).unwrap_or(0);

        let remaining_data = abi::encode_call_uint256("getRemaining(uint256)", classroom_id);
        let remaining_hex = self.eth_call(&remaining_data).await?;
        let remaining = abi::decode_uint64(&remaining_hex).unwrap_or(0);

        let spent_data = abi::encode_call_uint256("getSpent(uint256)", classroom_id);
        let spent_hex = self.eth_call(&spent_data).await?;
        let spent = abi::decode_uint64(&spent_hex).unwrap_or(0);

        let limit_data = abi::encode_call_uint256("getMonthlyLimit(uint256)", classroom_id);
        let limit_hex = self.eth_call(&limit_data).await?;
        let monthly_limit = abi::decode_uint64(&limit_hex).unwrap_or(0);

        Ok(BudgetInfo {
            classroom_id,
            allocated,
            remaining,
            spent,
            monthly_limit,
        })
    }

    /// Data source: BudgetAllocation.getRemaining(uint256) via eth_call
    async fn get_remaining(&self, classroom_id: u64) -> Result<u64, AppError> {
        let data = abi::encode_call_uint256("getRemaining(uint256)", classroom_id);
        let result = self.eth_call(&data).await?;
        abi::decode_uint64(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode remaining budget".to_string()))
    }

    /// Data source: BudgetAllocation.getSpent(uint256) via eth_call
    async fn get_spent(&self, classroom_id: u64) -> Result<u64, AppError> {
        let data = abi::encode_call_uint256("getSpent(uint256)", classroom_id);
        let result = self.eth_call(&data).await?;
        abi::decode_uint64(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode spent amount".to_string()))
    }
}
