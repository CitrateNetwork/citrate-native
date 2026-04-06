//! Institutional vault + cashout service.
//!
//! Data source: InstitutionalVault (0x8464...318bC) and CashoutRequest (0xbCF2...1508)
//! via eth_call on chain 40204.

use crate::error::AppError;
use super::abi;

/// Contract addresses on chain 40204 (deployed 2026-04-05).
const VAULT_ADDRESS: &str = "0x20Fbd46DeEd5EEDEB6e5c87eeB31924e9CA312ad";
const CASHOUT_ADDRESS: &str = "0x130A46b6E41DB6E1e18fb9c759F223c459190e90";

/// Vault status summary.
#[derive(Debug, Clone)]
pub struct VaultStatus {
    pub address: String,
    pub threshold: u64,
    pub signer_count: u64,
    pub is_paused: bool,
    pub balance_wei: String,
}

/// Cashout request info.
#[derive(Debug, Clone)]
pub struct CashoutInfo {
    pub id: u64,
    pub teacher: String,
    pub amount: String,
    pub status: CashoutStatus,
    pub classroom_id: u64,
}

/// Cashout lifecycle states.
#[derive(Debug, Clone, PartialEq)]
pub enum CashoutStatus {
    Pending,
    Approved,
    Rejected,
    Unknown(u8),
}

impl From<u8> for CashoutStatus {
    fn from(v: u8) -> Self {
        match v {
            0 => CashoutStatus::Pending,
            1 => CashoutStatus::Approved,
            2 => CashoutStatus::Rejected,
            other => CashoutStatus::Unknown(other),
        }
    }
}

/// Backend trait for institutional queries.
#[async_trait::async_trait]
pub trait InstitutionalBackend: Send + Sync {
    /// Get vault status (threshold, signers, pause state, balance).
    async fn get_vault_status(&self) -> Result<VaultStatus, AppError>;

    /// Check if an address is a vault signer.
    async fn is_signer(&self, address: &str) -> Result<bool, AppError>;

    /// Check if the vault is paused.
    async fn is_paused(&self) -> Result<bool, AppError>;

    /// Get cashout request status by ID.
    async fn get_cashout_status(&self, cashout_id: u64) -> Result<CashoutInfo, AppError>;

    /// Get the SALT/USD rate (basis points).
    async fn get_salt_usd_rate(&self) -> Result<u64, AppError>;
}

/// RPC-backed implementation querying real on-chain state.
pub struct RpcInstitutionalBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcInstitutionalBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Execute an eth_call against a contract and return the hex result.
    async fn eth_call(&self, to: &str, data: &str) -> Result<String, AppError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": to, "data": data}, "latest"],
            "id": 1,
        });

        let resp = self.client.post(&self.rpc_url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Vault RPC call failed: {}", e)))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| AppError::Network(format!("Vault response parse failed: {}", e)))?;

        json.get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let err_msg = json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                AppError::ContractCall {
                    contract: to.to_string(),
                    method: data[..10.min(data.len())].to_string(),
                    reason: err_msg.to_string(),
                }
            })
    }
}

#[async_trait::async_trait]
impl InstitutionalBackend for RpcInstitutionalBackend {
    /// Data source: InstitutionalVault.threshold() + signerCount() + isPaused() + eth_getBalance
    async fn get_vault_status(&self) -> Result<VaultStatus, AppError> {
        // threshold() — selector: cast sig "threshold()"
        let threshold_data = abi::encode_call("threshold()");
        let threshold_hex = self.eth_call(VAULT_ADDRESS, &threshold_data).await?;
        let threshold = abi::decode_uint256(&threshold_hex).unwrap_or(0);

        // signerCount() — selector
        let count_data = abi::encode_call("signerCount()");
        let count_hex = self.eth_call(VAULT_ADDRESS, &count_data).await?;
        let signer_count = abi::decode_uint256(&count_hex).unwrap_or(0);

        // isPaused()
        let paused_data = abi::encode_call("isPaused()");
        let paused_hex = self.eth_call(VAULT_ADDRESS, &paused_data).await?;
        let is_paused = abi::decode_bool(&paused_hex);

        // eth_getBalance for vault
        let balance_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBalance",
            "params": [VAULT_ADDRESS, "latest"],
            "id": 2,
        });
        let balance_resp = self.client.post(&self.rpc_url)
            .json(&balance_body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Balance RPC failed: {}", e)))?;
        let balance_json: serde_json::Value = balance_resp.json().await
            .map_err(|e| AppError::Network(format!("Balance parse failed: {}", e)))?;
        let balance_wei = balance_json.get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("0x0")
            .to_string();

        Ok(VaultStatus {
            address: VAULT_ADDRESS.to_string(),
            threshold,
            signer_count,
            is_paused,
            balance_wei,
        })
    }

    /// Data source: InstitutionalVault.isSigner(address) via eth_call
    async fn is_signer(&self, address: &str) -> Result<bool, AppError> {
        let data = abi::encode_call_address("isSigner(address)", address);
        let result = self.eth_call(VAULT_ADDRESS, &data).await?;
        Ok(abi::decode_bool(&result))
    }

    /// Data source: InstitutionalVault.isPaused() via eth_call
    async fn is_paused(&self) -> Result<bool, AppError> {
        let data = abi::encode_call("isPaused()");
        let result = self.eth_call(VAULT_ADDRESS, &data).await?;
        Ok(abi::decode_bool(&result))
    }

    /// Data source: CashoutRequest.getRequestStatus/Teacher/Amount(uint256) via eth_call
    async fn get_cashout_status(&self, cashout_id: u64) -> Result<CashoutInfo, AppError> {
        // getRequestStatus(uint256)
        let status_data = abi::encode_call_uint256("getRequestStatus(uint256)", cashout_id);
        let status_hex = self.eth_call(CASHOUT_ADDRESS, &status_data).await?;
        let status_code = abi::decode_uint8(&status_hex).unwrap_or(255);

        // getRequestTeacher(uint256)
        let teacher_data = abi::encode_call_uint256("getRequestTeacher(uint256)", cashout_id);
        let teacher_hex = self.eth_call(CASHOUT_ADDRESS, &teacher_data).await?;
        let teacher = abi::decode_address(&teacher_hex);

        // getRequestAmount(uint256)
        let amount_data = abi::encode_call_uint256("getRequestAmount(uint256)", cashout_id);
        let amount_hex = self.eth_call(CASHOUT_ADDRESS, &amount_data).await?;
        let amount = abi::decode_uint256(&amount_hex).unwrap_or(0);

        Ok(CashoutInfo {
            id: cashout_id,
            teacher,
            amount: amount.to_string(),
            status: CashoutStatus::from(status_code),
            classroom_id: 0, // Not stored in CashoutRequest struct getter
        })
    }

    /// Data source: CashoutRequest.getSaltUsdRate() via eth_call
    async fn get_salt_usd_rate(&self) -> Result<u64, AppError> {
        let data = abi::encode_call("getSaltUsdRate()");
        let result = self.eth_call(CASHOUT_ADDRESS, &data).await?;
        abi::decode_uint256(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode SALT/USD rate".to_string()))
    }
}
