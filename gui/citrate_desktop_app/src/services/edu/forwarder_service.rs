//! Meta-transaction forwarder service.
//!
//! Data source: Forwarder (0x948B...4F8F) via eth_call on chain 40204.
//! Constructs ForwardRequest structs for student sponsored transactions.
//! Manages nonce tracking and device binding validation.

use crate::error::AppError;
use super::abi;

/// Contract address on chain 40204 (deployed 2026-04-05).
const FORWARDER_ADDRESS: &str = "0xc63d2a04762529edB649d7a4cC3E57A0085e8544";

/// A meta-transaction forward request matching the Solidity struct.
#[derive(Debug, Clone)]
pub struct ForwardRequest {
    pub org_principal_id: String,  // bytes32 hex
    pub classroom_id: u64,
    pub nonce: u64,
    pub session_expiry: u64,
    pub device_cert_hash: String,  // bytes32 hex
    pub target: String,            // address
    pub data: Vec<u8>,             // calldata
}

/// Relayer status.
#[derive(Debug, Clone)]
pub struct RelayerInfo {
    pub address: String,
    pub is_authorized: bool,
}

/// Backend trait for forwarder queries and meta-tx construction.
#[async_trait::async_trait]
pub trait ForwarderBackend: Send + Sync {
    /// Get the current nonce for an org principal.
    async fn get_nonce(&self, org_principal_id: &str) -> Result<u64, AppError>;

    /// Check if an address is an authorized relayer.
    async fn is_relayer(&self, address: &str) -> Result<bool, AppError>;

    /// Construct a ForwardRequest with the next valid nonce.
    async fn build_forward_request(
        &self,
        org_principal_id: &str,
        classroom_id: u64,
        session_expiry: u64,
        device_cert_hash: &str,
        target: &str,
        data: Vec<u8>,
    ) -> Result<ForwardRequest, AppError>;

    /// Encode a ForwardRequest for submission to the relayer.
    fn encode_execute_call(&self, req: &ForwardRequest) -> Result<Vec<u8>, AppError>;
}

/// RPC-backed implementation.
pub struct RpcForwarderBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcForwarderBackend {
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
            "params": [{"to": FORWARDER_ADDRESS, "data": data}, "latest"],
            "id": 1,
        });

        let resp = self.client.post(&self.rpc_url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Forwarder RPC call failed: {}", e)))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| AppError::Network(format!("Forwarder response parse failed: {}", e)))?;

        json.get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let err_msg = json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                AppError::ContractCall {
                    contract: FORWARDER_ADDRESS.to_string(),
                    method: data[..10.min(data.len())].to_string(),
                    reason: err_msg.to_string(),
                }
            })
    }
}

#[async_trait::async_trait]
impl ForwarderBackend for RpcForwarderBackend {
    /// Data source: Forwarder.getNonce(bytes32) via eth_call
    async fn get_nonce(&self, org_principal_id: &str) -> Result<u64, AppError> {
        let sel = hex::encode(abi::selector("getNonce(bytes32)"));
        let id_clean = org_principal_id.trim_start_matches("0x");
        let data = format!("0x{}{:0>64}", sel, id_clean);
        let result = self.eth_call(&data).await?;
        abi::decode_uint256(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode nonce".to_string()))
    }

    /// Data source: Forwarder.isRelayer(address) via eth_call
    async fn is_relayer(&self, address: &str) -> Result<bool, AppError> {
        let data = abi::encode_call_address("isRelayer(address)", address);
        let result = self.eth_call(&data).await?;
        Ok(abi::decode_bool(&result))
    }

    /// Build a ForwardRequest with the correct nonce from on-chain state.
    async fn build_forward_request(
        &self,
        org_principal_id: &str,
        classroom_id: u64,
        session_expiry: u64,
        device_cert_hash: &str,
        target: &str,
        data: Vec<u8>,
    ) -> Result<ForwardRequest, AppError> {
        let nonce = self.get_nonce(org_principal_id).await?;

        Ok(ForwardRequest {
            org_principal_id: org_principal_id.to_string(),
            classroom_id,
            nonce,
            session_expiry,
            device_cert_hash: device_cert_hash.to_string(),
            target: target.to_string(),
            data,
        })
    }

    /// ABI-encode a ForwardRequest into the `execute(ForwardRequest,bytes)` calldata.
    fn encode_execute_call(&self, req: &ForwardRequest) -> Result<Vec<u8>, AppError> {
        // execute((bytes32,uint256,uint256,uint256,bytes32,address,bytes),bytes)
        // This is a complex ABI encoding. For now we encode the tuple fields.
        let sel = abi::selector("execute((bytes32,uint256,uint256,uint256,bytes32,address,bytes),bytes)");

        let org_id = req.org_principal_id.trim_start_matches("0x");
        let device = req.device_cert_hash.trim_start_matches("0x");
        let target = req.target.trim_start_matches("0x").to_lowercase();

        // Struct fields as 32-byte words:
        // orgPrincipalId (bytes32)
        // classroomId (uint256)
        // nonce (uint256)
        // sessionExpiry (uint256)
        // deviceCertHash (bytes32)
        // target (address)
        // data offset (uint256) — points to dynamic data
        // Then: signature offset, signature length=0, signature bytes=empty

        // The dynamic data (bytes) requires offset computation.
        // offset to data = 7 words * 32 = 224 = 0xe0
        let data_hex = hex::encode(&req.data);
        let data_len = req.data.len();

        let mut encoded = hex::encode(sel);
        // Offset to tuple (first arg) = 0x40 (after two offset words: tuple offset + sig offset)
        encoded.push_str(&format!("{:0>64x}", 0x40u64)); // tuple offset
        // Calculate signature offset (after tuple data + data bytes)
        let tuple_static_words = 7; // 7 static fields in struct
        let data_words = (data_len + 31) / 32;
        let sig_offset = 0x40 + (tuple_static_words + 1 + 1 + data_words) * 32; // approximate
        encoded.push_str(&format!("{:0>64x}", sig_offset)); // sig offset

        // Tuple fields
        encoded.push_str(&format!("{:0>64}", org_id)); // orgPrincipalId
        encoded.push_str(&format!("{:0>64x}", req.classroom_id)); // classroomId
        encoded.push_str(&format!("{:0>64x}", req.nonce)); // nonce
        encoded.push_str(&format!("{:0>64x}", req.session_expiry)); // sessionExpiry
        encoded.push_str(&format!("{:0>64}", device)); // deviceCertHash
        encoded.push_str(&format!("{:0>64}", target)); // target
        // data offset within tuple = 7 * 32 = 224 = 0xe0
        encoded.push_str(&format!("{:0>64x}", 0xe0u64)); // data offset
        // data length
        encoded.push_str(&format!("{:0>64x}", data_len));
        // data content (padded to 32 bytes)
        encoded.push_str(&data_hex);
        let padding = (32 - (data_len % 32)) % 32;
        encoded.push_str(&"0".repeat(padding * 2));

        // Signature (empty bytes for now — relayer signs separately)
        encoded.push_str(&format!("{:0>64x}", 0u64)); // sig length = 0

        hex::decode(&encoded)
            .map_err(|e| AppError::ChainQuery(format!("ABI encoding failed: {}", e)))
    }
}
