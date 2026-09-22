//! Meta-transaction forwarder service.
//!
//! Data source: Forwarder (0xcb5fcad3…056583e) via eth_call on chain 40204.
//! Constructs ForwardRequest structs for student sponsored transactions.
//! Manages nonce tracking and device binding validation.

use super::abi;
use crate::error::AppError;

/// Forwarder (EIP-2771) contract address on chain 40204.
///
/// RM-E.4 / GUI_NATIVE-2026-05-31-007: the single canonical Forwarder
/// address. Three divergent values had drifted across the tree (this
/// const, `citrate_native::marketplace_client`'s registry, and a
/// stale doc comment). Per federation-lead direction the most-current
/// value (`marketplace_client`, committed 2026-04-22) is canonical; the
/// others were aligned to it. `forwarder_address_matches_canonical` pins
/// it so future divergence fails CI. NOTE: not verified against an
/// on-chain deployment record (none in-tree) — deploy-time confirmation
/// is the close-gate.
const FORWARDER_ADDRESS: &str = "0xcb5fcad35f892e7e1da4bb4d17a48dd9e056583e";

/// A meta-transaction forward request matching the Solidity struct.
#[derive(Debug, Clone)]
pub struct ForwardRequest {
    pub org_principal_id: String, // bytes32 hex
    pub classroom_id: u64,
    pub nonce: u64,
    pub session_expiry: u64,
    pub device_cert_hash: String, // bytes32 hex
    pub target: String,           // address
    pub data: Vec<u8>,            // calldata
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

        let resp = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Forwarder RPC call failed: {}", e)))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::Network(format!("Forwarder response parse failed: {}", e)))?;

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
        // GUI_NATIVE-2026-05-31-002/003: validated bytes32, not string-padded.
        let id_clean = abi::require_hex(org_principal_id, 64, "org_principal_id")
            .map_err(AppError::ChainQuery)?;
        let data = format!("0x{}{}", sel, id_clean);
        let result = self.eth_call(&data).await?;
        abi::decode_uint64(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode nonce".to_string()))
    }

    /// Data source: Forwarder.isRelayer(address) via eth_call
    async fn is_relayer(&self, address: &str) -> Result<bool, AppError> {
        let data = abi::encode_call_address("isRelayer(address)", address)
            .map_err(AppError::ChainQuery)?;
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
    ///
    /// GUI_NATIVE-2026-05-31-002 (WP 6.4b): canonical ABI encoding. The
    /// previous version string-padded unvalidated fields and computed an
    /// "approximate" signature offset one word too large, so a relayer (or
    /// the Forwarder contract) decoding the calldata would read garbage.
    /// Layout, with all offsets exact:
    ///   head: [tuple offset = 0x40][sig offset = 0x40 + tuple_size]
    ///   tuple: 7 static words (data offset = 0xe0) + data length + padded data
    ///   sig:   length word (0 — the relayer attaches the signature)
    fn encode_execute_call(&self, req: &ForwardRequest) -> Result<Vec<u8>, AppError> {
        let sel =
            abi::selector("execute((bytes32,uint256,uint256,uint256,bytes32,address,bytes),bytes)");

        // Validated fixed-width hex — malformed fields err, never pad.
        let org_id = abi::require_hex(&req.org_principal_id, 64, "org_principal_id")
            .map_err(AppError::ChainQuery)?;
        let device = abi::require_hex(&req.device_cert_hash, 64, "device_cert_hash")
            .map_err(AppError::ChainQuery)?;
        let target =
            abi::require_hex(&req.target, 40, "target address").map_err(AppError::ChainQuery)?;

        let data_hex = hex::encode(&req.data);
        let data_len = req.data.len();
        let data_padded = data_len.div_ceil(32) * 32;

        // tuple = 7 static words + data length word + padded data bytes.
        let tuple_size = 7 * 32 + 32 + data_padded;
        let sig_offset = 0x40 + tuple_size;

        let mut encoded = hex::encode(sel);
        encoded.push_str(&format!("{:0>64x}", 0x40u64)); // tuple offset
        encoded.push_str(&format!("{:0>64x}", sig_offset)); // sig offset (exact)

        // Tuple fields
        encoded.push_str(&format!("{:0>64}", org_id)); // orgPrincipalId (bytes32)
        encoded.push_str(&format!("{:0>64x}", req.classroom_id)); // classroomId
        encoded.push_str(&format!("{:0>64x}", req.nonce)); // nonce
        encoded.push_str(&format!("{:0>64x}", req.session_expiry)); // sessionExpiry
        encoded.push_str(&format!("{:0>64}", device)); // deviceCertHash (bytes32)
        encoded.push_str(&format!("{:0>64}", target)); // target (address, left-padded)
                                                       // data offset within the tuple = 7 * 32 = 0xe0
        encoded.push_str(&format!("{:0>64x}", 0xe0u64));
        // data length + right-padded content
        encoded.push_str(&format!("{:0>64x}", data_len));
        encoded.push_str(&data_hex);
        encoded.push_str(&"0".repeat((data_padded - data_len) * 2));

        // Signature (empty bytes — the relayer signs separately)
        encoded.push_str(&format!("{:0>64x}", 0u64)); // sig length = 0

        hex::decode(&encoded)
            .map_err(|e| AppError::ChainQuery(format!("ABI encoding failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RM-E.4 / GUI_NATIVE-007 tripwire: pin the canonical Forwarder
    /// address so it cannot silently drift again. The value is the
    /// most-current of the three that had diverged (the
    /// `marketplace_client` registry value, committed 2026-04-22). If a
    /// future edit changes this const, CI fails and the change must be
    /// re-justified against the canonical deployment.
    // ── GUI_NATIVE-2026-05-31-002 (WP 6.4b) ─────────────────────────────

    fn sample_request(data: Vec<u8>) -> ForwardRequest {
        ForwardRequest {
            org_principal_id: format!("0x{}", "11".repeat(32)),
            classroom_id: 7,
            nonce: 3,
            session_expiry: 99,
            device_cert_hash: format!("0x{}", "22".repeat(32)),
            target: format!("0x{}", "33".repeat(20)),
            data,
        }
    }

    fn word_u64(enc: &[u8], word_idx: usize) -> u64 {
        let w = &enc[4 + word_idx * 32..4 + (word_idx + 1) * 32];
        assert!(
            w[..24].iter().all(|b| *b == 0),
            "word {word_idx} overflows u64"
        );
        u64::from_be_bytes(w[24..32].try_into().unwrap())
    }

    /// The `execute((bytes32,uint256,uint256,uint256,bytes32,address,bytes),bytes)`
    /// encoding must be canonical ABI: head = [tuple offset, sig offset], the
    /// tuple's dynamic `data` at 0xe0 within the tuple, and the signature
    /// offset pointing exactly past the tuple (NOT "approximate").
    #[test]
    fn execute_encoding_is_canonical_abi() {
        let backend = RpcForwarderBackend::new("http://127.0.0.1:1");
        let data = vec![0xde, 0xad, 0xbe, 0xef, 0x01]; // 5 bytes → pads to 32
        let enc = backend
            .encode_execute_call(&sample_request(data))
            .expect("encodes");

        // Head: tuple offset, then signature offset.
        assert_eq!(word_u64(&enc, 0), 0x40, "tuple offset");
        // tuple = 7 head words + data length word + 1 padded data word
        //       = 7*32 + 32 + 32 = 288; sig offset = 0x40 + 288 = 0x160.
        assert_eq!(
            word_u64(&enc, 1),
            0x160,
            "signature offset must point just past the tuple"
        );

        // Tuple fields (words 2..=8).
        assert_eq!(word_u64(&enc, 3), 7, "classroomId");
        assert_eq!(word_u64(&enc, 4), 3, "nonce");
        assert_eq!(word_u64(&enc, 5), 99, "sessionExpiry");
        assert_eq!(word_u64(&enc, 8), 0xe0, "data offset within the tuple");
        assert_eq!(word_u64(&enc, 9), 5, "data length");

        // Signature: empty bytes (length word 0) at the declared offset.
        let sig_len_pos = 4 + 0x160;
        assert!(enc[sig_len_pos..sig_len_pos + 32].iter().all(|b| *b == 0));
        // Nothing after — total length is exact.
        assert_eq!(enc.len(), 4 + 0x160 + 32, "no trailing or missing bytes");
    }

    /// Struct fields must be validated hex of the exact width — short or
    /// non-hex `bytes32`/`address` inputs are refused, not string-padded.
    #[test]
    fn execute_encoding_rejects_malformed_fields() {
        let backend = RpcForwarderBackend::new("http://127.0.0.1:1");

        let mut short_org = sample_request(vec![]);
        short_org.org_principal_id = "0xabcd".into();
        assert!(
            backend.encode_execute_call(&short_org).is_err(),
            "short org_principal_id"
        );

        let mut bad_device = sample_request(vec![]);
        bad_device.device_cert_hash = format!("0x{}", "zz".repeat(32));
        assert!(
            backend.encode_execute_call(&bad_device).is_err(),
            "non-hex device_cert_hash"
        );

        let mut short_target = sample_request(vec![]);
        short_target.target = "0x1234".into();
        assert!(
            backend.encode_execute_call(&short_target).is_err(),
            "short target address"
        );
    }

    #[test]
    fn forwarder_address_matches_canonical() {
        assert_eq!(
            FORWARDER_ADDRESS, "0xcb5fcad35f892e7e1da4bb4d17a48dd9e056583e",
            "Forwarder address drifted — re-confirm the canonical chain-40204 \
             deployment before changing it (GUI_NATIVE-007)"
        );
        // Sanity: 0x + 40 lowercase hex.
        assert!(FORWARDER_ADDRESS.starts_with("0x"));
        assert_eq!(FORWARDER_ADDRESS.len(), 42);
        assert!(FORWARDER_ADDRESS[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }
}
