//! Block service — query blocks, transactions, and DAG structure.
//!
//! Data source: eth_getBlockByNumber, eth_getBlockByHash,
//! citrate_getDagTips, citrate_getDagStats RPC methods.

use crate::error::AppError;
use crate::event_bus::EventBus;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Block info for display.
#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub height: u64,
    pub hash: String,
    pub parent_hash: String,
    pub timestamp: u64,
    pub tx_count: usize,
    pub blue_score: u64,
    pub proposer: String,
}

/// Transaction info for display.
#[derive(Debug, Clone)]
pub struct TxInfo {
    pub hash: String,
    pub from: String,
    pub to: String,
    pub value: String,
    pub status: String, // "confirmed", "pending", "failed"
    pub block_height: u64,
    pub tx_type: String, // "transfer", "deploy", "inference", "training"
}

/// Backend trait.
#[async_trait::async_trait]
pub trait BlockBackend: Send + Sync {
    async fn get_block_by_number(&self, number: u64) -> Result<BlockInfo, AppError>;
    async fn get_recent_blocks(&self, count: usize) -> Result<Vec<BlockInfo>, AppError>;
    async fn get_block_transactions(&self, block_hash: &str) -> Result<Vec<TxInfo>, AppError>;
    async fn get_transaction(&self, tx_hash: &str) -> Result<TxInfo, AppError>;
}

/// Real RPC backend.
pub struct RpcBlockBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcBlockBackend {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl BlockBackend for RpcBlockBackend {
    async fn get_block_by_number(&self, number: u64) -> Result<BlockInfo, AppError> {
        let hex_num = format!("0x{:x}", number);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getBlockByNumber",
            "params": [hex_num, false],
            "id": 1,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Block RPC failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Block parse failed: {}", e)))?;

        let result = json
            .get("result")
            .ok_or_else(|| AppError::ChainQuery("No block result".to_string()))?;

        Ok(BlockInfo {
            height: number,
            hash: result
                .get("hash")
                .and_then(|h| h.as_str())
                .unwrap_or("0x")
                .to_string(),
            parent_hash: result
                .get("parentHash")
                .and_then(|h| h.as_str())
                .unwrap_or("0x")
                .to_string(),
            timestamp: result
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0),
            tx_count: result
                .get("transactions")
                .and_then(|t| t.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            blue_score: 0,
            proposer: result
                .get("miner")
                .and_then(|m| m.as_str())
                .unwrap_or("0x")
                .to_string(),
        })
    }

    async fn get_recent_blocks(&self, count: usize) -> Result<Vec<BlockInfo>, AppError> {
        // Get current block number
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1,
        });

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Network(format!("BlockNumber RPC failed: {}", e)))?;

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Network(format!("BlockNumber parse failed: {}", e)))?;

        let hex = json
            .pointer("/result")
            .and_then(|r| r.as_str())
            .unwrap_or("0x0");
        let current = u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0);

        let mut blocks = Vec::new();
        let start = current.saturating_sub(count as u64);
        for i in (start..=current).rev().take(count) {
            match self.get_block_by_number(i).await {
                Ok(block) => blocks.push(block),
                Err(_) => break,
            }
        }
        Ok(blocks)
    }

    async fn get_block_transactions(&self, _block_hash: &str) -> Result<Vec<TxInfo>, AppError> {
        Ok(Vec::new()) // Will be wired when block detail is needed
    }

    async fn get_transaction(&self, _tx_hash: &str) -> Result<TxInfo, AppError> {
        Err(AppError::ChainQuery(
            "Transaction lookup not yet wired".to_string(),
        ))
    }
}

#[cfg(test)]
pub struct TestBlockBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl BlockBackend for TestBlockBackend {
    async fn get_block_by_number(&self, number: u64) -> Result<BlockInfo, AppError> {
        Ok(BlockInfo {
            height: number,
            hash: format!("0x{:064x}", number),
            parent_hash: format!("0x{:064x}", number.saturating_sub(1)),
            timestamp: 1711555200 + number,
            tx_count: (number % 5) as usize,
            blue_score: number,
            proposer: "0x9f5B156C53305D4b20c94ca08E3219D1C0e7401a".to_string(),
        })
    }
    async fn get_recent_blocks(&self, count: usize) -> Result<Vec<BlockInfo>, AppError> {
        let mut blocks = Vec::new();
        for i in (0..count).rev() {
            blocks.push(self.get_block_by_number(100 - i as u64).await?);
        }
        Ok(blocks)
    }
    async fn get_block_transactions(&self, _hash: &str) -> Result<Vec<TxInfo>, AppError> {
        Ok(vec![TxInfo {
            hash: "0xabc".to_string(),
            from: "0x1234".to_string(),
            to: "0x5678".to_string(),
            value: "1000000000000000000".to_string(),
            status: "confirmed".to_string(),
            block_height: 100,
            tx_type: "transfer".to_string(),
        }])
    }
    async fn get_transaction(&self, hash: &str) -> Result<TxInfo, AppError> {
        Ok(TxInfo {
            hash: hash.to_string(),
            from: "0x1234".to_string(),
            to: "0x5678".to_string(),
            value: "1000000000000000000".to_string(),
            status: "confirmed".to_string(),
            block_height: 100,
            tx_type: "transfer".to_string(),
        })
    }
}

/// Block service.
// Infrastructure fields (events) used when real-time event publishing is wired.
#[allow(dead_code)]
pub struct BlockService {
    events: Arc<EventBus>,
    backend: Arc<dyn BlockBackend>,
    cached_blocks: Arc<RwLock<Vec<BlockInfo>>>,
}

impl BlockService {
    pub fn new(events: Arc<EventBus>, rpc_url: &str) -> Self {
        Self {
            events,
            backend: Arc::new(RpcBlockBackend::new(rpc_url)),
            cached_blocks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn BlockBackend>) -> Self {
        Self {
            events,
            backend,
            cached_blocks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn refresh_blocks(&self, count: usize) -> Result<Vec<BlockInfo>, AppError> {
        let blocks = self.backend.get_recent_blocks(count).await?;
        *self.cached_blocks.write().await = blocks.clone();
        Ok(blocks)
    }

    pub async fn get_cached_blocks(&self) -> Vec<BlockInfo> {
        self.cached_blocks.read().await.clone()
    }

    pub async fn get_block(&self, number: u64) -> Result<BlockInfo, AppError> {
        self.backend.get_block_by_number(number).await
    }

    pub async fn get_block_transactions(&self, block_hash: &str) -> Result<Vec<TxInfo>, AppError> {
        self.backend.get_block_transactions(block_hash).await
    }

    pub async fn get_transaction(&self, tx_hash: &str) -> Result<TxInfo, AppError> {
        self.backend.get_transaction(tx_hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> BlockService {
        let events = Arc::new(EventBus::new());
        BlockService::with_backend(events, Arc::new(TestBlockBackend))
    }

    #[tokio::test]
    async fn test_refresh_blocks() {
        let svc = test_service();
        let blocks = svc.refresh_blocks(5).await.expect("refresh");
        assert_eq!(blocks.len(), 5);
    }

    #[tokio::test]
    async fn test_get_block() {
        let svc = test_service();
        let block = svc.get_block(42).await.expect("get block");
        assert_eq!(block.height, 42);
        assert!(!block.hash.is_empty());
    }

    #[tokio::test]
    async fn test_cached_blocks() {
        let svc = test_service();
        assert!(svc.get_cached_blocks().await.is_empty());
        svc.refresh_blocks(3).await.expect("refresh");
        assert_eq!(svc.get_cached_blocks().await.len(), 3);
    }

    #[tokio::test]
    async fn test_block_has_parent() {
        let svc = test_service();
        let block = svc.get_block(50).await.expect("get block");
        assert!(!block.parent_hash.is_empty());
    }

    #[tokio::test]
    async fn test_get_block_transactions() {
        let svc = test_service();
        let txs = svc.get_block_transactions("0xabc").await.expect("get txs");
        assert!(!txs.is_empty());
    }

    #[tokio::test]
    async fn test_get_transaction() {
        let svc = test_service();
        let tx = svc.get_transaction("0xhash").await.expect("get tx");
        assert_eq!(tx.hash, "0xhash");
        assert_eq!(tx.status, "confirmed");
    }
}
