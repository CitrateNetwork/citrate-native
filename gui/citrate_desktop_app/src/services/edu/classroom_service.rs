//! Classroom cluster + device management service.
//!
//! Data source: ClassroomClusterV1 (0x71C9...292e) via eth_call on chain 40204.
//! Covers: classroom CRUD, org/classroom roles, device registry, student transfers.

use crate::error::AppError;
use super::abi;

/// Contract address on chain 40204 (deployed 2026-04-05).
const CLUSTER_ADDRESS: &str = "0x71C95911E9a5D330f4D621842EC243EE1343292e";

/// Org-level role (maps to ClassroomClusterV1.OrgRole enum).
#[derive(Debug, Clone, PartialEq)]
pub enum OrgRole {
    None,
    Admin,
    IT,
    SuperAdmin,
    Unknown(u8),
}

impl From<u8> for OrgRole {
    fn from(v: u8) -> Self {
        match v {
            0 => OrgRole::None,
            1 => OrgRole::Admin,
            2 => OrgRole::IT,
            3 => OrgRole::SuperAdmin,
            other => OrgRole::Unknown(other),
        }
    }
}

/// Classroom-level role.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassroomRole {
    None,
    Student,
    TA,
    Teacher,
    Unknown(u8),
}

impl From<u8> for ClassroomRole {
    fn from(v: u8) -> Self {
        match v {
            0 => ClassroomRole::None,
            1 => ClassroomRole::Student,
            2 => ClassroomRole::TA,
            3 => ClassroomRole::Teacher,
            other => ClassroomRole::Unknown(other),
        }
    }
}

/// Classroom summary info.
#[derive(Debug, Clone)]
pub struct ClassroomInfo {
    pub id: u64,
    pub name: String,
    pub teacher: String,
    pub student_count: u64,
}

/// Device registry entry.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub cert_hash: String,
    pub user_address: String,
    pub is_active: bool,
}

/// Backend trait for classroom and device queries.
#[async_trait::async_trait]
pub trait ClassroomBackend: Send + Sync {
    /// Get classroom info by ID.
    async fn get_classroom(&self, classroom_id: u64) -> Result<ClassroomInfo, AppError>;

    /// Get the student count for a classroom.
    async fn get_student_count(&self, classroom_id: u64) -> Result<u64, AppError>;

    /// Get a user's org role.
    async fn get_org_role(&self, address: &str) -> Result<OrgRole, AppError>;

    /// Get a user's role in a specific classroom.
    async fn get_classroom_role(&self, classroom_id: u64, address: &str) -> Result<ClassroomRole, AppError>;

    /// Check if a device cert is active.
    async fn is_device_active(&self, cert_hash: &str) -> Result<bool, AppError>;

    /// Get the user bound to a device cert.
    async fn get_device_user(&self, cert_hash: &str) -> Result<String, AppError>;
}

/// RPC-backed implementation.
pub struct RpcClassroomBackend {
    rpc_url: String,
    client: reqwest::Client,
}

impl RpcClassroomBackend {
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
            "params": [{"to": CLUSTER_ADDRESS, "data": data}, "latest"],
            "id": 1,
        });

        let resp = self.client.post(&self.rpc_url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("Cluster RPC call failed: {}", e)))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| AppError::Network(format!("Cluster response parse failed: {}", e)))?;

        json.get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let err_msg = json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                AppError::ContractCall {
                    contract: CLUSTER_ADDRESS.to_string(),
                    method: data[..10.min(data.len())].to_string(),
                    reason: err_msg.to_string(),
                }
            })
    }
}

#[async_trait::async_trait]
impl ClassroomBackend for RpcClassroomBackend {
    /// Data source: ClassroomClusterV1.getClassroomName/Teacher(uint256) + getStudentCount(uint256)
    async fn get_classroom(&self, classroom_id: u64) -> Result<ClassroomInfo, AppError> {
        // getClassroomTeacher(uint256)
        let teacher_data = abi::encode_call_uint256("getClassroomTeacher(uint256)", classroom_id);
        let teacher_hex = self.eth_call(&teacher_data).await?;
        let teacher = abi::decode_address(&teacher_hex);

        // getStudentCount(uint256)
        let count_data = abi::encode_call_uint256("getStudentCount(uint256)", classroom_id);
        let count_hex = self.eth_call(&count_data).await?;
        let student_count = abi::decode_uint256(&count_hex).unwrap_or(0);

        // Note: getClassroomName returns a string which needs ABI string decoding.
        // For now, we return the classroom ID as the name placeholder.
        // Full string decoding would require parsing offset+length+data from the result.

        Ok(ClassroomInfo {
            id: classroom_id,
            name: format!("Classroom #{}", classroom_id),
            teacher,
            student_count,
        })
    }

    /// Data source: ClassroomClusterV1.getStudentCount(uint256) via eth_call
    async fn get_student_count(&self, classroom_id: u64) -> Result<u64, AppError> {
        let data = abi::encode_call_uint256("getStudentCount(uint256)", classroom_id);
        let result = self.eth_call(&data).await?;
        abi::decode_uint256(&result)
            .ok_or_else(|| AppError::ChainQuery("Failed to decode student count".to_string()))
    }

    /// Data source: ClassroomClusterV1.getOrgRole(address) via eth_call
    async fn get_org_role(&self, address: &str) -> Result<OrgRole, AppError> {
        let data = abi::encode_call_address("getOrgRole(address)", address);
        let result = self.eth_call(&data).await?;
        let code = abi::decode_uint8(&result).unwrap_or(0);
        Ok(OrgRole::from(code))
    }

    /// Data source: ClassroomClusterV1.getClassroomRole(uint256,address) via eth_call
    async fn get_classroom_role(&self, classroom_id: u64, address: &str) -> Result<ClassroomRole, AppError> {
        let data = abi::encode_call_uint256_address(
            "getClassroomRole(uint256,address)", classroom_id, address,
        );
        let result = self.eth_call(&data).await?;
        let code = abi::decode_uint8(&result).unwrap_or(0);
        Ok(ClassroomRole::from(code))
    }

    /// Data source: ClassroomClusterV1.isDeviceActive(bytes32) via eth_call
    async fn is_device_active(&self, cert_hash: &str) -> Result<bool, AppError> {
        // isDeviceActive(bytes32) — cert_hash is already a bytes32 hex string
        let sel = hex::encode(abi::selector("isDeviceActive(bytes32)"));
        let hash_clean = cert_hash.trim_start_matches("0x");
        let data = format!("0x{}{:0>64}", sel, hash_clean);
        let result = self.eth_call(&data).await?;
        Ok(abi::decode_bool(&result))
    }

    /// Data source: ClassroomClusterV1.getDeviceUser(bytes32) via eth_call
    async fn get_device_user(&self, cert_hash: &str) -> Result<String, AppError> {
        let sel = hex::encode(abi::selector("getDeviceUser(bytes32)"));
        let hash_clean = cert_hash.trim_start_matches("0x");
        let data = format!("0x{}{:0>64}", sel, hash_clean);
        let result = self.eth_call(&data).await?;
        Ok(abi::decode_address(&result))
    }
}
