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
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// Default port — configurable via AppConfig.mcp_port. Picked from
/// the ephemeral range above the node RPC ports (8545) and below the
/// IPFS API (5001 already taken). 9600 is unlikely to clash.
pub const DEFAULT_MCP_PORT: u16 = 9600;

/// Auth-token TTL when the GUI issues one without an explicit override.
/// 30 days matches typical session-token TTLs and bounds the damage if
/// a token leaks but operators don't notice.
/// RM-B1 / WP-E1.1 (audit GUI-C-04).
const DEFAULT_AUTH_TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;

/// A pre-issued authentication token bound to a maximum policy.
/// MCP clients pass `auth_token` at `initialize`; the server resolves
/// the policy from this server-side record rather than trusting the
/// caller-supplied policy field.
///
/// RM-B1 / WP-E1.1 (audit GUI-C-04). Pre-fix `handle_initialize`
/// took the policy directly from JSON-RPC params, letting any
/// connecting client claim `Maintainer`. Post-fix the policy is
/// bounded by what the operator pre-authorized via the Operations
/// panel's "Generate MCP access token" flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    /// The opaque secret string presented at `initialize`. 256 bits
    /// of entropy hex-encoded.
    pub token: String,
    /// The maximum policy this token may grant. Initialize requests
    /// asking for a more permissive policy are downgraded to this
    /// ceiling (or rejected, depending on the strict-mode flag).
    pub max_policy: PolicyProfile,
    /// Unix timestamp at issuance.
    pub created_at: u64,
    /// Unix timestamp after which the token no longer authorizes
    /// `initialize`.
    pub expires_at: u64,
    /// Operator-facing label so the Operations panel can identify
    /// tokens by purpose ("local Hermes", "Claude Desktop", etc.)
    /// rather than by opaque hex.
    pub label: String,
    /// Set to true when the operator revokes the token via the panel.
    pub revoked: bool,
}

/// UI-friendly view of [`AuthToken`] (no secret material).
#[derive(Debug, Clone, Serialize)]
pub struct AuthTokenSummary {
    pub label: String,
    pub max_policy: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub revoked: bool,
    /// First 8 hex chars of the token, for visual matching against
    /// the value the operator copy-pasted.
    pub token_prefix: String,
}

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
    /// Operator-issued auth tokens, keyed by the secret token string.
    /// RM-B1 / WP-E1.1 (audit GUI-C-04).
    tokens: RwLock<HashMap<String, AuthToken>>,
    /// On-disk path for token persistence. `None` means in-memory
    /// only (used in tests).
    tokens_file: Option<PathBuf>,
}

impl McpHostService {
    pub fn new(mcp: Arc<McpServer>) -> Self {
        Self {
            mcp,
            endpoint: RwLock::new(String::new()),
            listening: RwLock::new(false),
            tokens: RwLock::new(HashMap::new()),
            tokens_file: None,
        }
    }

    /// Construct a host whose tokens are loaded from + persisted to
    /// `data_dir/mcp_tokens.json`. The file is created with 0600
    /// permissions on Unix to keep it out of reach of other local
    /// users.
    /// RM-B1 / WP-E1.2 (audit GUI-C-04).
    pub fn with_token_storage(mcp: Arc<McpServer>, data_dir: PathBuf) -> Self {
        let tokens_file = data_dir.join("mcp_tokens.json");
        let initial = load_tokens_from_disk(&tokens_file);
        Self {
            mcp,
            endpoint: RwLock::new(String::new()),
            listening: RwLock::new(false),
            tokens: RwLock::new(initial),
            tokens_file: Some(tokens_file),
        }
    }

    /// Issue a new auth token with the given maximum policy. Operators
    /// generate one of these from the Operations panel and copy it
    /// into the MCP client's config (Hermes / Claude Desktop / etc.).
    pub async fn create_token(
        &self,
        label: impl Into<String>,
        max_policy: PolicyProfile,
        ttl_secs: Option<u64>,
    ) -> AuthToken {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = now + ttl_secs.unwrap_or(DEFAULT_AUTH_TOKEN_TTL_SECS);
        let record = AuthToken {
            token: token.clone(),
            max_policy,
            created_at: now,
            expires_at,
            label: label.into(),
            revoked: false,
        };
        {
            let mut tokens = self.tokens.write().await;
            tokens.insert(token.clone(), record.clone());
            self.persist_tokens(&tokens).await;
        }
        record
    }

    /// Revoke a token by prefix or full string. Returns true when a
    /// token was removed.
    pub async fn revoke_token(&self, token_or_prefix: &str) -> bool {
        let mut tokens = self.tokens.write().await;
        let target_key = tokens
            .keys()
            .find(|k| k.as_str() == token_or_prefix || k.starts_with(token_or_prefix))
            .cloned();
        if let Some(key) = target_key {
            if let Some(t) = tokens.get_mut(&key) {
                t.revoked = true;
            }
            self.persist_tokens(&tokens).await;
            true
        } else {
            false
        }
    }

    /// UI listing — never exposes the raw secret beyond an 8-char
    /// prefix.
    pub async fn list_tokens(&self) -> Vec<AuthTokenSummary> {
        let tokens = self.tokens.read().await;
        tokens
            .values()
            .map(|t| AuthTokenSummary {
                label: t.label.clone(),
                max_policy: format!("{:?}", t.max_policy),
                created_at: t.created_at,
                expires_at: t.expires_at,
                revoked: t.revoked,
                token_prefix: t.token.chars().take(8).collect(),
            })
            .collect()
    }

    /// Internal: validate a presented token and return its record,
    /// or `None` if missing/expired/revoked.
    async fn lookup_active_token(&self, token: &str) -> Option<AuthToken> {
        let tokens = self.tokens.read().await;
        let record = tokens.get(token)?.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if record.revoked || record.expires_at <= now {
            return None;
        }
        Some(record)
    }

    async fn persist_tokens(&self, tokens: &HashMap<String, AuthToken>) {
        let Some(path) = self.tokens_file.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let entries: Vec<&AuthToken> = tokens.values().collect();
        match serde_json::to_vec_pretty(&entries) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, &bytes) {
                    tracing::warn!("MCP host: failed to persist tokens: {}", e);
                    return;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
            Err(e) => tracing::warn!("MCP host: token serialization failed: {}", e),
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

/// Load tokens from disk, returning an empty map if the file is
/// missing or malformed (a corrupt file should not brick the host).
fn load_tokens_from_disk(path: &PathBuf) -> HashMap<String, AuthToken> {
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    let entries: Vec<AuthToken> = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("MCP host: tokens file corrupt, ignoring: {}", e);
            return HashMap::new();
        }
    };
    entries.into_iter().map(|t| (t.token.clone(), t)).collect()
}

/// Pick the more restrictive of two policies. ReadOnly < Guided <
/// Operator < Maintainer.
fn cap_policy(requested: PolicyProfile, ceiling: PolicyProfile) -> PolicyProfile {
    fn rank(p: &PolicyProfile) -> u8 {
        match p {
            PolicyProfile::ReadOnly => 0,
            PolicyProfile::Guided => 1,
            PolicyProfile::Operator => 2,
            PolicyProfile::Maintainer => 3,
        }
    }
    if rank(&requested) <= rank(&ceiling) {
        requested
    } else {
        ceiling
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
///
/// RM-B1 / WP-E5.8 (audit AGT-13): the transport-layer
/// `Authorization: Bearer <token>` header is honored when present
/// — clients that authenticate at the transport layer are exempt
/// from passing `auth_token` again in the JSON-RPC params. Either
/// is sufficient. The `initialize` handler still upholds the
/// server-bounded policy from WP-E1.1.
async fn dispatch(
    State(host): State<Arc<McpHostService>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    if req.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::err(req.id, -32600, "Expected jsonrpc: 2.0"));
    }

    // Extract bearer token from the Authorization header if present.
    let bearer_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    // For initialize, splice the bearer token into params.auth_token
    // so the existing server-bounded-policy code path picks it up
    // without changes. Only override when params don't already
    // carry an auth_token (caller's explicit field wins).
    let mut params = req.params;
    if let Some(token) = bearer_token.clone() {
        if let Some(obj) = params.as_object_mut() {
            if !obj.contains_key("auth_token") {
                obj.insert(
                    "auth_token".to_string(),
                    serde_json::Value::String(token),
                );
            }
        }
    }

    match req.method.as_str() {
        "initialize" => handle_initialize(&host, req.id, params).await,
        "tools/list" => handle_tools_list(&host, req.id, params).await,
        "session/end" => handle_session_end(&host, req.id, params).await,
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
    let requested_policy = match params.get("policy").and_then(|v| v.as_str()).unwrap_or("ReadOnly") {
        "ReadOnly" => PolicyProfile::ReadOnly,
        "Guided" => PolicyProfile::Guided,
        "Operator" => PolicyProfile::Operator,
        "Maintainer" => PolicyProfile::Maintainer,
        other => return Json(JsonRpcResponse::err(id, -32602, format!("Unknown policy: {}", other))),
    };

    // RM-B1 / WP-E1.1 (audit GUI-C-04): server-bounded policy.
    // Pre-fix the caller's `policy` field was trusted verbatim — any
    // local process could initialize with `Maintainer` and obtain
    // full tool access. Post-fix the policy is bounded by an
    // operator-issued auth_token. No token → ReadOnly only.
    let auth_token = params
        .get("auth_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let policy = match auth_token.as_deref() {
        Some(tok) => match host.lookup_active_token(tok).await {
            Some(record) => cap_policy(requested_policy, record.max_policy),
            None => {
                return Json(JsonRpcResponse::err(
                    id,
                    -32001,
                    "auth_token unknown, expired, or revoked",
                ));
            }
        },
        None => {
            // No token → operator hasn't authorized this client. We
            // accept the connection at ReadOnly so tool discovery
            // still works (Hermes / Claude Desktop introspection),
            // but any escalation request is silently downgraded.
            PolicyProfile::ReadOnly
        }
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
        // RM-B1 / WP-E5.1 (audit AGT-06): grants minted directly by
        // the GUI host carry no signature today — the host IS the
        // issuer, and the auth_token presented at initialize is the
        // operator-authorized credential. Production strict-mode
        // wallets MUST sign here; tracked as a follow-on on the
        // RM-G2 backlog so the cutover is observed end-to-end.
        issuer_pubkey: Vec::new(),
        signature: Vec::new(),
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

    // ── RM-E1 / WP-E1.1 (audit GUI-C-04) ────────────────────────────

    /// Without `auth_token`, initialize must downgrade ANY requested
    /// policy to `ReadOnly`. A caller can no longer claim Maintainer
    /// without operator authorization.
    #[tokio::test]
    async fn test_guic04_no_token_caps_at_readonly() {
        let host = test_host();
        let params = serde_json::json!({
            "policy": "Maintainer",
            "recipient": "attacker-client"
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        let body = resp.0;
        // Request is accepted, but the issued grant is bounded.
        assert!(body.error.is_none(), "no-token requests are accepted at ReadOnly");
        let grants = host.list_active_grants().await;
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].policy, format!("{:?}", PolicyProfile::ReadOnly));
    }

    /// An invalid auth_token must be rejected outright (not silently
    /// downgraded — that would mask operator misconfiguration).
    #[tokio::test]
    async fn test_guic04_unknown_token_rejected() {
        let host = test_host();
        let params = serde_json::json!({
            "auth_token": "00".repeat(32),
            "policy": "Operator"
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        let body = resp.0;
        assert!(body.error.is_some(), "unknown auth_token must error");
        assert_eq!(body.error.as_ref().unwrap().code, -32001);
        let grants = host.list_active_grants().await;
        assert_eq!(grants.len(), 0, "no grant on rejection");
    }

    /// A valid token capped at Guided must downgrade a Maintainer
    /// request to Guided (not reject; the client's discovery still
    /// works at the lower scope).
    #[tokio::test]
    async fn test_guic04_token_caps_above_max() {
        let host = test_host();
        let token = host.create_token("test", PolicyProfile::Guided, None).await;
        let params = serde_json::json!({
            "auth_token": token.token,
            "policy": "Maintainer",
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_none());
        let grants = host.list_active_grants().await;
        assert_eq!(grants[0].policy, format!("{:?}", PolicyProfile::Guided));
    }

    /// A token at Operator capacity allows Operator initialize to
    /// pass through unchanged.
    #[tokio::test]
    async fn test_guic04_token_at_capacity_passes_through() {
        let host = test_host();
        let token = host
            .create_token("test", PolicyProfile::Operator, None)
            .await;
        let params = serde_json::json!({
            "auth_token": token.token,
            "policy": "Operator",
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_none());
        let grants = host.list_active_grants().await;
        assert_eq!(grants[0].policy, format!("{:?}", PolicyProfile::Operator));
    }

    /// Revoked tokens must be rejected.
    #[tokio::test]
    async fn test_guic04_revoked_token_rejected() {
        let host = test_host();
        let token = host
            .create_token("revocable", PolicyProfile::Operator, None)
            .await;
        assert!(host.revoke_token(&token.token).await);

        let params = serde_json::json!({
            "auth_token": token.token,
            "policy": "Operator",
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_some(), "revoked token must error");
    }

    /// Expired tokens must be rejected.
    #[tokio::test]
    async fn test_guic04_expired_token_rejected() {
        let host = test_host();
        // TTL = 0 → expires_at == created_at (already in the past).
        let token = host
            .create_token("expired", PolicyProfile::Operator, Some(0))
            .await;
        let params = serde_json::json!({
            "auth_token": token.token,
            "policy": "Operator",
        });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_some(), "expired token must error");
    }

    /// `list_tokens` exposes only the prefix, never the full secret.
    #[tokio::test]
    async fn test_guic04_token_listing_redacts_secret() {
        let host = test_host();
        let token = host
            .create_token("hermes-local", PolicyProfile::Guided, None)
            .await;
        let summaries = host.list_tokens().await;
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.label, "hermes-local");
        assert_eq!(s.token_prefix.len(), 8);
        assert!(token.token.starts_with(&s.token_prefix));
        // Summary serialization MUST NOT include the full token.
        let json = serde_json::to_string(s).expect("ser");
        assert!(!json.contains(&token.token), "raw token leaked in summary");
    }

    /// File-backed token storage round-trips through restart.
    #[tokio::test]
    async fn test_guic04_token_storage_persists_across_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(McpServer::new(registry.clone()));
        let host =
            Arc::new(McpHostService::with_token_storage(mcp, tmp.path().to_path_buf()));
        let token = host
            .create_token("persistent", PolicyProfile::Operator, None)
            .await;

        // New host instance reading the same on-disk file.
        let mcp2 = Arc::new(McpServer::new(registry));
        let host2 = Arc::new(McpHostService::with_token_storage(
            mcp2,
            tmp.path().to_path_buf(),
        ));
        let summaries = host2.list_tokens().await;
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].label, "persistent");

        // The token still validates after the "restart".
        let params = serde_json::json!({
            "auth_token": token.token,
            "policy": "Operator",
        });
        let resp = handle_initialize(&host2, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_none());
    }

    /// On Unix, the persisted file is mode 0600.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_guic04_token_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(McpServer::new(registry));
        let host =
            Arc::new(McpHostService::with_token_storage(mcp, tmp.path().to_path_buf()));
        host.create_token("perm-check", PolicyProfile::ReadOnly, None)
            .await;
        let path = tmp.path().join("mcp_tokens.json");
        let mode = std::fs::metadata(&path)
            .expect("file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "tokens file must be 0600");
    }
}
