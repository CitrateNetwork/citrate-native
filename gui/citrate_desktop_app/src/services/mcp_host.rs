//! MCP host — the GUI-side transport that external agent runtimes
//! (Hermes, Claude-with-MCP-client, local agent frameworks) connect
//! to. Wraps `citrate_agent_core::McpServer` with an axum JSON-RPC
//! 2.0 listener bound to 127.0.0.1:{mcp_port}.
//!
//! v1 methods (P960-J):
//!   - `initialize` — client announces policy + skill-pack URL;
//!     server assigns a grant_id, returns server capabilities.
//!   - `tools/list` — client asks what tools are exposed under its
//!     grant's policy; server replies with filtered descriptors.
//!   - `session/end` — client hangs up cleanly; server removes
//!     the grant.
//!
//! v1 does NOT implement `tools/call` — invoking a tool via MCP
//! requires the existing PendingApprovalStore integration, per-grant
//! rate limiting, and long-running future lifecycle tracking. That's
//! P960-K. v1 gives Hermes *discovery* of Citrate's tools; the actual
//! invocation still happens through the in-GUI chat+approval flow.
//!
//! The transport binds **127.0.0.1 only**. Cross-host Hermes clients
//! are explicitly out of scope for v1 — that's a separate security
//! review (TLS, auth, CORS).

use crate::error::AppError;
use axum::{
    extract::State,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use citrate_agent_core::canonical::{CapabilityGrant, PolicyProfile};
use citrate_agent_core::mcp_server::McpServer;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// Default port — configurable via AppConfig.mcp_port. Picked from
/// the ephemeral range above the node RPC ports (8545) and below the
/// IPFS API (5001 already taken). 9600 is unlikely to clash.
pub const DEFAULT_MCP_PORT: u16 = 9600;

/// User-facing summary of a single MCP session, for the Operations
/// panel to render.
#[derive(Debug, Clone, Serialize)]
pub struct GrantSummary {
    pub id: String,
    pub policy: String,
    pub recipient: String,
    pub connected_since: u64,
    pub tool_count: u32,
}

/// Live status of the MCP host — what the Operations panel displays.
#[derive(Debug, Clone)]
pub struct McpHostStatus {
    pub listening: bool,
    pub endpoint: String,
    pub active_sessions: Vec<GrantSummary>,
}

pub struct McpHostService {
    mcp: Arc<McpServer>,
    /// Listening endpoint (e.g., "http://127.0.0.1:9600"). Empty
    /// when not yet started or when bind failed.
    endpoint: RwLock<String>,
    /// Whether `start()` succeeded.
    listening: RwLock<bool>,
}

impl McpHostService {
    pub fn new(mcp: Arc<McpServer>) -> Self {
        Self {
            mcp,
            endpoint: RwLock::new(String::new()),
            listening: RwLock::new(false),
        }
    }

    /// Start the HTTP listener on 127.0.0.1:port. Spawns the
    /// listener on the current tokio runtime. Returns once the
    /// bind succeeds (or fails fast with AppError on bind error).
    pub async fn start(self: Arc<Self>, port: u16) -> Result<(), AppError> {
        let addr = format!("127.0.0.1:{}", port);
        let listener = TcpListener::bind(&addr).await.map_err(|e| {
            AppError::Network(format!("MCP host bind {} failed: {}", addr, e))
        })?;
        let endpoint = format!("http://{}", addr);
        tracing::info!("MCP host listening on {}", endpoint);
        *self.endpoint.write().await = endpoint;
        *self.listening.write().await = true;
        let router = Router::new()
            .route("/mcp", post(dispatch))
            .with_state(self.clone());
        // Spawn the axum server. Errors inside the serve loop just
        // log — we don't propagate them because the GUI should keep
        // running. The `listening` flag is a one-shot set to true at
        // bind time; actual loop failure is visible to clients (and
        // the status poll) via connection refused.
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("MCP host serve loop ended: {}", e);
            }
        });
        Ok(())
    }

    /// Snapshot of the host's current state for the Operations panel.
    pub async fn status(&self) -> McpHostStatus {
        let endpoint = self.endpoint.read().await.clone();
        let listening = *self.listening.read().await;
        let active_sessions = self.list_active_grants().await;
        McpHostStatus {
            listening,
            endpoint,
            active_sessions,
        }
    }

    /// List all non-revoked grants as UI-friendly summaries.
    pub async fn list_active_grants(&self) -> Vec<GrantSummary> {
        self.mcp.snapshot_grants().await
            .into_iter()
            .filter(|g| !g.revoked)
            .map(|g| GrantSummary {
                id: g.id.clone(),
                policy: format!("{:?}", g.policy),
                recipient: g.recipient.clone(),
                connected_since: g.connected_since,
                tool_count: g.allowed_tools.len() as u32,
            })
            .collect()
    }

    /// Generate a sidecar config JSON that a Hermes client can load
    /// to connect. If `grant_id` is None, returns a template the
    /// user must fill in with the value from `initialize`. v1 always
    /// issues ReadOnly policy — that's the scope Hermes operates
    /// under for tool discovery.
    pub async fn export_sidecar_config(&self, grant_id: Option<String>) -> serde_json::Value {
        let endpoint = self.endpoint.read().await.clone();
        serde_json::json!({
            "mcp_endpoint": endpoint,
            "grant_id": grant_id.unwrap_or_default(),
            "policy": "ReadOnly",
            "skill_pack": "",
            "logseq_sync": false,
            "benchmark_export": false,
            "notes": "Call POST /mcp with {jsonrpc:2.0,method:initialize,params:{policy:ReadOnly},id:1} to obtain a grant_id; then paste it above.",
        })
    }
}

// ── JSON-RPC 2.0 types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
    #[serde(default)]
    id: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0", result: Some(result), error: None, id }
    }
    fn err(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(JsonRpcError { code, message: message.into() }),
            id,
        }
    }
}

/// Single entry point — all JSON-RPC methods dispatch from here.
async fn dispatch(
    State(host): State<Arc<McpHostService>>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    if req.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::err(req.id, -32600, "Expected jsonrpc: 2.0"));
    }
    match req.method.as_str() {
        "initialize" => handle_initialize(&host, req.id, req.params).await,
        "tools/list" => handle_tools_list(&host, req.id, req.params).await,
        "session/end" => handle_session_end(&host, req.id, req.params).await,
        "tools/call" => Json(JsonRpcResponse::err(
            req.id,
            -32601,
            "tools/call not supported in v1 — use Citrate's in-GUI chat for tool invocation. Tracked as P960-K.",
        )),
        other => Json(JsonRpcResponse::err(
            req.id,
            -32601,
            format!("Method not found: {}", other),
        )),
    }
}

async fn handle_initialize(
    host: &Arc<McpHostService>,
    id: serde_json::Value,
    params: serde_json::Value,
) -> Json<JsonRpcResponse> {
    let policy = match params.get("policy").and_then(|v| v.as_str()).unwrap_or("ReadOnly") {
        "ReadOnly" => PolicyProfile::ReadOnly,
        "Guided" => PolicyProfile::Guided,
        "Operator" => PolicyProfile::Operator,
        "Maintainer" => PolicyProfile::Maintainer,
        other => return Json(JsonRpcResponse::err(id, -32602, format!("Unknown policy: {}", other))),
    };
    let recipient = params.get("recipient")
        .and_then(|v| v.as_str())
        .unwrap_or("hermes-client")
        .to_string();

    let grant_id = uuid::Uuid::new_v4().to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 8-hour expiry matches the GUI's wallet-session TTL. Clients
    // are expected to re-initialize if they need longer sessions.
    let expires = now + 8 * 3600;
    let expires_rfc3339 = chrono::DateTime::<chrono::Utc>::from_timestamp(expires as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "".to_string());

    let tool_descriptors = host.mcp.list_tools(&policy).await;
    let tool_names: Vec<String> = tool_descriptors.iter().map(|d| d.name.clone()).collect();
    let tool_count = tool_names.len() as u32;

    let grant = CapabilityGrant {
        id: grant_id.clone(),
        issuer: "citrate-gui".to_string(),
        recipient,
        allowed_tools: tool_names,
        max_value_per_tx: None,
        allowed_paths: Vec::new(),
        expires_at: expires_rfc3339,
        policy,
        revoked: false,
        connected_since: now,
    };
    host.mcp.add_grant(grant).await;
    tracing::info!("MCP host: grant {} registered", &grant_id[..8]);

    Json(JsonRpcResponse::ok(id, serde_json::json!({
        "grant_id": grant_id,
        "server_version": env!("CARGO_PKG_VERSION"),
        "tool_count": tool_count,
        "expires_at": expires,
    })))
}

async fn handle_tools_list(
    host: &Arc<McpHostService>,
    id: serde_json::Value,
    params: serde_json::Value,
) -> Json<JsonRpcResponse> {
    let grant_id = match params.get("grant_id").and_then(|v| v.as_str()) {
        Some(g) => g.to_string(),
        None => return Json(JsonRpcResponse::err(id, -32602, "Missing grant_id")),
    };
    // Look up the grant
    let grants = host.mcp.snapshot_grants().await;
    let Some(grant) = grants.iter().find(|g| g.id == grant_id && !g.revoked) else {
        return Json(JsonRpcResponse::err(id, -32001, "Unknown or revoked grant_id"));
    };
    let descriptors = host.mcp.list_tools(&grant.policy).await;
    // Serialize each tool definition; McpToolDefinition is already Serialize
    let tools: Vec<serde_json::Value> = descriptors
        .into_iter()
        .map(|d| serde_json::to_value(&d).unwrap_or(serde_json::Value::Null))
        .collect();
    Json(JsonRpcResponse::ok(id, serde_json::json!({ "tools": tools })))
}

async fn handle_session_end(
    host: &Arc<McpHostService>,
    id: serde_json::Value,
    params: serde_json::Value,
) -> Json<JsonRpcResponse> {
    let grant_id = match params.get("grant_id").and_then(|v| v.as_str()) {
        Some(g) => g.to_string(),
        None => return Json(JsonRpcResponse::err(id, -32602, "Missing grant_id")),
    };
    let removed = host.mcp.revoke_grant(&grant_id).await;
    if removed {
        tracing::info!("MCP host: grant {} ended", &grant_id[..8.min(grant_id.len())]);
        Json(JsonRpcResponse::ok(id, serde_json::json!({ "ok": true })))
    } else {
        Json(JsonRpcResponse::err(id, -32001, "Grant not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use citrate_agent_core::tool::ToolRegistry;

    fn test_host() -> Arc<McpHostService> {
        let registry = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(McpServer::new(registry));
        Arc::new(McpHostService::new(mcp))
    }

    #[tokio::test]
    async fn status_before_start() {
        let host = test_host();
        let s = host.status().await;
        assert!(!s.listening);
        assert_eq!(s.endpoint, "");
        assert_eq!(s.active_sessions.len(), 0);
    }

    #[tokio::test]
    async fn sidecar_config_shape() {
        let host = test_host();
        let cfg = host.export_sidecar_config(None).await;
        assert_eq!(cfg["policy"], "ReadOnly");
        assert_eq!(cfg["grant_id"], "");
        assert!(cfg["mcp_endpoint"].is_string());
    }

    #[tokio::test]
    async fn sidecar_config_with_grant_id() {
        let host = test_host();
        let cfg = host.export_sidecar_config(Some("abc-123".to_string())).await;
        assert_eq!(cfg["grant_id"], "abc-123");
    }
}
