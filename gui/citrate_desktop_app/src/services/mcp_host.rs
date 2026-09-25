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
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use citrate_agent_core::canonical::{CapabilityGrant, PolicyProfile};
use citrate_agent_core::mcp_server::McpServer;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

// ── ENCRYPT-S1 WP-4: token vault (inventory A6) ─────────────────────
//
// `mcp_tokens.json` used to be plaintext JSON — bearer tokens at rest,
// protected only by 0600. It is now AES-256-GCM ciphertext under a
// 32-byte master key held in the OS keyring (macOS Keychain / Windows
// Credential Manager / Linux native keystore), mirroring the
// citrate-comms `keyvault` pattern. On-disk layout:
//
//   MAGIC(8) ‖ nonce(12) ‖ AES-256-GCM ciphertext(JSON token array)
//
// A legacy plaintext file found at load time is migrated in place:
// parsed, best-effort shredded (overwritten with zeros + fsync +
// removed), and rewritten sealed. If no keyring is available
// (headless), tokens are held in memory only for the session — never
// written back as plaintext.

/// Keyring service — matches `SystemSecretStore` in `ports/mod.rs` so
/// all of the desktop app's secrets live under one service name.
pub const TOKEN_VAULT_KEYRING_SERVICE: &str = "citrate-desktop";
/// Keyring account for the token-vault master key.
pub const TOKEN_VAULT_KEYRING_ACCOUNT: &str = "mcp-token-vault-master-key";
/// File-format magic for the sealed tokens file (version 1).
const TOKEN_VAULT_MAGIC: &[u8; 8] = b"CITMCPV1";
/// AES-GCM nonce length.
const TOKEN_VAULT_NONCE_LEN: usize = 12;

/// Load the token-vault master key from the OS keyring, generating and
/// storing one on first run (citrate-comms keyvault shape).
///
/// The key is cached process-wide after the first successful load: the
/// keyring is hit once per process, and every `McpHostService` in the
/// process seals/opens with the same key. (Failures are NOT cached, so
/// a transient keyring hiccup doesn't stick for the process lifetime.)
fn load_or_create_master_key() -> Result<[u8; 32], String> {
    static CACHED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    if let Some(key) = CACHED.get() {
        return Ok(*key);
    }
    let key = load_master_key_from_keyring()?;
    Ok(*CACHED.get_or_init(|| key))
}

/// The uncached keyring get-or-create.
fn load_master_key_from_keyring() -> Result<[u8; 32], String> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        let entry = keyring::Entry::new(TOKEN_VAULT_KEYRING_SERVICE, TOKEN_VAULT_KEYRING_ACCOUNT)
            .map_err(|e| format!("keyring entry: {e}"))?;
        match entry.get_secret() {
            Ok(bytes) => bytes
                .as_slice()
                .try_into()
                .map_err(|_| "stored master key is not 32 bytes".to_string()),
            Err(keyring::Error::NoEntry) => {
                let mut key = [0u8; 32];
                rand::thread_rng().fill_bytes(&mut key);
                entry
                    .set_secret(&key)
                    .map_err(|e| format!("keyring write: {e}"))?;
                Ok(key)
            }
            Err(e) => Err(format!("keyring read: {e}")),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("OS keyring unsupported on this platform".to_string())
    }
}

/// Constant-time byte-slice equality. Differing lengths are not secret
/// here (token length is fixed), so a length mismatch short-circuits;
/// equal-length inputs are compared without a data-dependent branch.
/// NAT-B-023: bearer-credential comparisons should not leak via timing.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Seal a plaintext token-array JSON blob: MAGIC ‖ nonce ‖ ciphertext.
///
/// NAT-B-024: the MAGIC is bound as AES-GCM associated data so the format
/// version cannot be swapped under the same key (downgrade) and a
/// ciphertext is not portable to a different-magic context. AAD is not
/// stored — it is re-derived from the same constant on open.
fn seal_token_blob(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, AeadCore, OsRng, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit};
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: TOKEN_VAULT_MAGIC.as_slice(),
            },
        )
        .map_err(|e| format!("token vault encrypt: {e}"))?;
    let mut out = Vec::with_capacity(TOKEN_VAULT_MAGIC.len() + nonce.len() + ciphertext.len());
    out.extend_from_slice(TOKEN_VAULT_MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Open a sealed tokens file back to the plaintext JSON blob.
fn open_token_blob(key: &[u8; 32], bytes: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let body = bytes
        .strip_prefix(TOKEN_VAULT_MAGIC.as_slice())
        .ok_or_else(|| "not a sealed token vault (bad magic)".to_string())?;
    if body.len() < TOKEN_VAULT_NONCE_LEN {
        return Err("sealed token vault truncated".to_string());
    }
    let (nonce, ciphertext) = body.split_at(TOKEN_VAULT_NONCE_LEN);
    let cipher = Aes256Gcm::new(key.into());
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: TOKEN_VAULT_MAGIC.as_slice(),
            },
        )
        .map_err(|e| format!("token vault decrypt: {e}"))
}

/// Parse the (plaintext) JSON token array into the in-memory map.
/// `None` = malformed (as opposed to a valid-but-empty array).
fn parse_token_entries(bytes: &[u8]) -> Option<HashMap<String, AuthToken>> {
    let entries: Vec<AuthToken> = serde_json::from_slice(bytes).ok()?;
    Some(entries.into_iter().map(|t| (t.token.clone(), t)).collect())
}

/// Best-effort shred: overwrite the plaintext bytes with zeros, fsync,
/// then unlink. (On CoW/journaled filesystems the old extents may
/// survive — this is hygiene, not a guarantee; the real fix is that new
/// writes are ciphertext-only.)
fn shred_plaintext_file(path: &Path, len: usize) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.write_all(&vec![0u8; len]);
        let _ = f.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

/// Serialize + seal + write the tokens file (0600 on Unix).
fn write_sealed_tokens(path: &Path, key: &[u8; 32], tokens: &HashMap<String, AuthToken>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entries: Vec<&AuthToken> = tokens.values().collect();
    let plaintext = match serde_json::to_vec(&entries) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("MCP host: token serialization failed: {}", e);
            return;
        }
    };
    let sealed = match seal_token_blob(key, &plaintext) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("MCP host: {}", e);
            return;
        }
    };
    // NAT-B-024: create the file 0600 BEFORE writing so the sealed bytes
    // never transit a umask-default (0644) window. On Unix, open with the
    // mode set at creation time; elsewhere fall back to write-then-chmod.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(&sealed) {
                    tracing::warn!("MCP host: failed to persist tokens: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("MCP host: failed to open token vault for write: {}", e);
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = std::fs::write(path, &sealed) {
            tracing::warn!("MCP host: failed to persist tokens: {}", e);
        }
    }
}

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
    /// only (tests, or headless hosts without an OS keyring).
    tokens_file: Option<PathBuf>,
    /// ENCRYPT-S1 WP-4: AES-256-GCM master key from the OS keyring.
    /// `None` means no keyring → no on-disk persistence (in-memory
    /// tokens only; we never fall back to writing plaintext).
    master_key: Option<[u8; 32]>,
    /// GUI_NATIVE-2026-05-31-005 (WP 6.4b): grant ownership — which
    /// auth_token (if any) minted each grant. `session/end` for a
    /// token-minted grant requires the SAME token; `None` (tokenless
    /// ReadOnly discovery grants) keeps the v1 grant-id-as-credential
    /// behavior.
    grant_owners: RwLock<HashMap<String, Option<String>>>,
}

impl McpHostService {
    pub fn new(mcp: Arc<McpServer>) -> Self {
        Self {
            mcp,
            endpoint: RwLock::new(String::new()),
            listening: RwLock::new(false),
            tokens: RwLock::new(HashMap::new()),
            tokens_file: None,
            master_key: None,
            grant_owners: RwLock::new(HashMap::new()),
        }
    }

    /// Construct a host whose tokens are loaded from + persisted to
    /// `data_dir/mcp_tokens.json`. The file is created with 0600
    /// permissions on Unix to keep it out of reach of other local
    /// users.
    /// RM-B1 / WP-E1.2 (audit GUI-C-04).
    ///
    /// ENCRYPT-S1 WP-4: the file is AES-256-GCM ciphertext under an
    /// OS-keyring master key. A legacy plaintext file is migrated
    /// (sealed + plaintext shredded) on first load. Without a usable
    /// keyring, tokens stay in-memory for the session (with a WARN) —
    /// plaintext is never written back.
    pub fn with_token_storage(mcp: Arc<McpServer>, data_dir: PathBuf) -> Self {
        let tokens_file = data_dir.join("mcp_tokens.json");
        let (initial, tokens_file, master_key) = match load_or_create_master_key() {
            Ok(key) => {
                let initial = load_tokens_from_disk(&tokens_file, &key);
                (initial, Some(tokens_file), Some(key))
            }
            Err(e) => {
                tracing::warn!(
                    "MCP host: OS keyring unavailable ({e}); auth tokens will be held \
                     in memory only for this session and will NOT persist across restarts"
                );
                // Best-effort continuity: surface tokens from a legacy
                // plaintext file so existing clients keep working, but
                // leave the file untouched (we cannot re-seal it) and
                // never persist back to it.
                let initial = std::fs::read(&tokens_file)
                    .ok()
                    .filter(|b| !b.starts_with(TOKEN_VAULT_MAGIC))
                    .and_then(|b| parse_token_entries(&b))
                    .unwrap_or_default();
                (initial, None, None)
            }
        };
        Self {
            mcp,
            endpoint: RwLock::new(String::new()),
            listening: RwLock::new(false),
            tokens: RwLock::new(initial),
            tokens_file,
            master_key,
            grant_owners: RwLock::new(HashMap::new()),
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
    ///
    /// NAT-B-023: a prefix match on an empty or ultra-short argument used to
    /// revoke an arbitrary token — `"".starts_with("")` is true for the first
    /// HashMap-ordered key, so `revoke_token("")` from the Operations panel
    /// silently revoked *some* token. Require the argument to be at least as
    /// long as the 8-char prefix the UI surfaces, prefer a constant-time full
    /// match, and only fall back to a prefix match at or above that length.
    pub async fn revoke_token(&self, token_or_prefix: &str) -> bool {
        const MIN_REVOKE_LEN: usize = 8;
        if token_or_prefix.len() < MIN_REVOKE_LEN {
            return false;
        }
        let mut tokens = self.tokens.write().await;
        let target_key = tokens
            .keys()
            .find(|k| {
                ct_eq(k.as_bytes(), token_or_prefix.as_bytes())
                    || (token_or_prefix.len() < k.len() && k.starts_with(token_or_prefix))
            })
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

    /// ENCRYPT-S1 WP-4: persistence is ciphertext-only. No master key
    /// (headless/no-keyring) → in-memory only, nothing touches disk.
    async fn persist_tokens(&self, tokens: &HashMap<String, AuthToken>) {
        let (Some(path), Some(key)) = (self.tokens_file.as_ref(), self.master_key.as_ref()) else {
            return;
        };
        write_sealed_tokens(path, key, tokens);
    }

    /// Start the HTTP listener on 127.0.0.1:port. Spawns the
    /// listener on the current tokio runtime. Returns once the
    /// bind succeeds (or fails fast with AppError on bind error).
    pub async fn start(self: Arc<Self>, port: u16) -> Result<(), AppError> {
        let addr = format!("127.0.0.1:{}", port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| AppError::Network(format!("MCP host bind {} failed: {}", addr, e)))?;
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
        self.mcp
            .snapshot_grants()
            .await
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
///
/// ENCRYPT-S1 WP-4: understands both formats —
/// - sealed vault (magic prefix): decrypt with the keyring master key;
/// - legacy plaintext JSON: parse, then MIGRATE in place — the
///   plaintext is shredded (zero-overwrite + fsync + unlink) and the
///   same path is rewritten as ciphertext.
fn load_tokens_from_disk(path: &PathBuf, master_key: &[u8; 32]) -> HashMap<String, AuthToken> {
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    if bytes.starts_with(TOKEN_VAULT_MAGIC) {
        match open_token_blob(master_key, &bytes).and_then(|pt| {
            parse_token_entries(&pt).ok_or_else(|| "sealed payload not a token array".into())
        }) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!("MCP host: sealed tokens file unreadable, ignoring: {}", e);
                HashMap::new()
            }
        }
    } else {
        // Legacy plaintext file (pre-ENCRYPT-S1).
        let Some(map) = parse_token_entries(&bytes) else {
            tracing::warn!("MCP host: tokens file corrupt, ignoring");
            return HashMap::new();
        };
        tracing::warn!(
            "MCP host: legacy plaintext tokens file found at {} — migrating to \
             keyring-sealed ciphertext and shredding the plaintext",
            path.display()
        );
        shred_plaintext_file(path, bytes.len());
        write_sealed_tokens(path, master_key, &map);
        map
    }
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
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        }
    }
    fn err(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
            id,
        }
    }
}

/// NAT-B-011: DNS-rebinding / cross-origin guard for the loopback MCP
/// host. The server binds `127.0.0.1` only, but a browser page can rebind
/// a hostname to `127.0.0.1` and become same-origin, then drive the JSON-
/// RPC surface. A native MCP client (Hermes / Claude Desktop) never sends
/// an `Origin`, and its `Host` is loopback; a rebinding page fails both.
/// Pure so it is unit-tested directly.
fn is_local_request(origin: Option<&str>, host: Option<&str>) -> bool {
    // Any Origin header at all means a web context — refuse it.
    if origin.is_some() {
        return false;
    }
    // If a Host header is present it must name loopback. (Absent Host is
    // allowed — HTTP/1.0 / direct socket clients.)
    match host {
        None => true,
        Some(h) => {
            let h = h.trim();
            let hostname = if let Some(rest) = h.strip_prefix('[') {
                // `[::1]` or `[::1]:port`
                rest.split_once(']').map(|(inner, _)| inner).unwrap_or(rest)
            } else {
                h.rsplit_once(':').map(|(hn, _)| hn).unwrap_or(h)
            };
            hostname == "127.0.0.1"
                || hostname == "localhost"
                || hostname == "::1"
                || hostname
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// PBA-L7b-015: the longest `recipient` label an `initialize` may carry.
const MAX_RECIPIENT_LEN: usize = 128;
/// PBA-L7b-015: live (unrevoked, unexpired) grants a TOKENLESS client may hold at once.
const MAX_ACTIVE_TOKENLESS_GRANTS: usize = 16;
/// PBA-L7b-015: hard ceiling on grants held in memory (the agent-runtime grant store only marks
/// revoked grants, it never drops them, so the host bounds the total it will ever mint).
const MAX_TOTAL_GRANTS: usize = 1024;

/// PBA-L7b-015: admission control for `initialize`. Pure so it is unit-tested directly.
fn admit_initialize(
    total_grants: usize,
    active_tokenless: usize,
    tokenless: bool,
) -> Result<(), &'static str> {
    if total_grants >= MAX_TOTAL_GRANTS {
        return Err("MCP session limit reached for this app run; restart Citrate to reset");
    }
    if tokenless && active_tokenless >= MAX_ACTIVE_TOKENLESS_GRANTS {
        return Err("too many open unauthenticated MCP sessions; end one or use an operator token");
    }
    Ok(())
}

/// NAT-B-011: `tools/list` must not serve a grant past its expiry. The
/// grant carries `expires_at` as an RFC-3339 string; compare it to `now`.
/// A grant with an unparseable timestamp is treated as expired (fail
/// closed). Pure so it is unit-tested directly.
fn grant_is_expired(expires_at_rfc3339: &str, now_secs: u64) -> bool {
    // Empty string means "never expires" per the CapabilityGrant contract.
    if expires_at_rfc3339.is_empty() {
        return false;
    }
    match chrono::DateTime::parse_from_rfc3339(expires_at_rfc3339) {
        Ok(dt) => (dt.timestamp().max(0) as u64) <= now_secs,
        Err(_) => true,
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    // NAT-B-011: DNS-rebinding / cross-origin guard before any work.
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let host_hdr = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    if !is_local_request(origin, host_hdr) {
        return Json(JsonRpcResponse::err(
            req.id,
            -32600,
            "cross-origin or non-loopback request refused (DNS-rebinding guard)",
        ));
    }

    if req.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::err(
            req.id,
            -32600,
            "Expected jsonrpc: 2.0",
        ));
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
                obj.insert("auth_token".to_string(), serde_json::Value::String(token));
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
    let requested_policy = match params
        .get("policy")
        .and_then(|v| v.as_str())
        .unwrap_or("ReadOnly")
    {
        "ReadOnly" => PolicyProfile::ReadOnly,
        "Guided" => PolicyProfile::Guided,
        "Operator" => PolicyProfile::Operator,
        "Maintainer" => PolicyProfile::Maintainer,
        other => {
            return Json(JsonRpcResponse::err(
                id,
                -32602,
                format!("Unknown policy: {}", other),
            ))
        }
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

    let recipient = params
        .get("recipient")
        .and_then(|v| v.as_str())
        .unwrap_or("hermes-client")
        .to_string();
    // PBA-L7b-015: bound what one `initialize` can store. Pre-fix the recipient was unbounded
    // (a ~1.9 MB string per call) and every tokenless call pushed a grant that was never pruned,
    // so a local loop exhausted memory.
    if recipient.len() > MAX_RECIPIENT_LEN {
        return Json(JsonRpcResponse::err(
            id,
            -32602,
            format!("recipient longer than {MAX_RECIPIENT_LEN} bytes"),
        ));
    }
    {
        let now = now_unix_secs();
        let grants = host.mcp.snapshot_grants().await;
        let owners = host.grant_owners.read().await;
        let active_tokenless = grants
            .iter()
            .filter(|g| !g.revoked && !grant_is_expired(&g.expires_at, now))
            .filter(|g| matches!(owners.get(&g.id), Some(None)))
            .count();
        if let Err(msg) = admit_initialize(grants.len(), active_tokenless, auth_token.is_none()) {
            return Json(JsonRpcResponse::err(id, -32005, msg));
        }
    }

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
        .unwrap_or_default();

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
    // GUI_NATIVE-2026-05-31-005: remember which credential minted this grant
    // so `session/end` can require the same one.
    // NAT-B-011: reap owner entries whose grant no longer exists in the
    // live set, so a client looping `initialize` cannot grow `grant_owners`
    // without bound (the external server caps/expires the grants themselves).
    {
        let live: std::collections::HashSet<String> = host
            .mcp
            .snapshot_grants()
            .await
            .into_iter()
            .map(|g| g.id)
            .collect();
        let mut owners = host.grant_owners.write().await;
        owners.retain(|gid, _| live.contains(gid) || *gid == grant_id);
        owners.insert(grant_id.clone(), auth_token.clone());
    }
    tracing::info!("MCP host: grant {} registered", &grant_id[..8]);

    Json(JsonRpcResponse::ok(
        id,
        serde_json::json!({
            "grant_id": grant_id,
            "server_version": env!("CARGO_PKG_VERSION"),
            "tool_count": tool_count,
            "expires_at": expires,
        }),
    ))
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
        return Json(JsonRpcResponse::err(
            id,
            -32001,
            "Unknown or revoked grant_id",
        ));
    };
    // NAT-B-011: enforce grant expiry here — `initialize` sets `expires_at`
    // but `tools/list` used to serve the tool inventory indefinitely.
    if grant_is_expired(&grant.expires_at, now_unix_secs()) {
        return Json(JsonRpcResponse::err(id, -32001, "grant_id has expired"));
    }
    let descriptors = host.mcp.list_tools(&grant.policy).await;
    // Serialize each tool definition; McpToolDefinition is already Serialize
    let tools: Vec<serde_json::Value> = descriptors
        .into_iter()
        .map(|d| serde_json::to_value(&d).unwrap_or(serde_json::Value::Null))
        .collect();
    Json(JsonRpcResponse::ok(
        id,
        serde_json::json!({ "tools": tools }),
    ))
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
    // GUI_NATIVE-2026-05-31-005 (WP 6.4b): caller-ownership binding. A grant
    // minted under an operator auth_token can only be ended by a caller
    // presenting that token (params field or transport bearer — dispatch
    // splices the bearer in). Without this, any local client that learns a
    // grant_id (e.g. from the Operations panel) could revoke someone else's
    // session — a grant DoS. Tokenless ReadOnly grants keep the v1
    // grant-id-as-credential behavior.
    let presented = params.get("auth_token").and_then(|v| v.as_str());
    {
        let owners = host.grant_owners.read().await;
        if let Some(Some(owner_token)) = owners.get(&grant_id) {
            // NAT-B-023: constant-time compare of the bearer credential.
            let ok = presented
                .map(|p| ct_eq(p.as_bytes(), owner_token.as_bytes()))
                .unwrap_or(false);
            if !ok {
                return Json(JsonRpcResponse::err(
                    id,
                    -32001,
                    "session/end requires the auth_token that created this grant",
                ));
            }
        }
    }
    let removed = host.mcp.revoke_grant(&grant_id).await;
    if removed {
        host.grant_owners.write().await.remove(&grant_id);
        tracing::info!(
            "MCP host: grant {} ended",
            &grant_id[..8.min(grant_id.len())]
        );
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

    /// ENCRYPT-S1 WP-4: route all keyring access in this test binary to
    /// the in-memory mock keystore — tests must never touch the real OS
    /// keychain. Process-global, idempotent via Once.
    fn use_mock_keyring() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
    }

    fn storage_host(dir: &std::path::Path) -> Arc<McpHostService> {
        use_mock_keyring();
        let registry = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(McpServer::new(registry));
        Arc::new(McpHostService::with_token_storage(mcp, dir.to_path_buf()))
    }

    /// PBA-L7b-015: a local loop of tokenless `initialize` calls cannot grow the grant store
    /// without bound, and an oversized recipient is refused before anything is stored.
    #[tokio::test]
    async fn pba_l7b_015_tokenless_initialize_is_bounded() {
        let host = test_host();
        let huge = "x".repeat(1_900_000);
        let resp = handle_initialize(
            &host,
            serde_json::json!(1),
            serde_json::json!({ "recipient": huge }),
        )
        .await;
        assert!(
            resp.0.error.is_some(),
            "oversized recipient must be refused"
        );
        assert_eq!(host.mcp.snapshot_grants().await.len(), 0, "nothing stored");

        let mut refused = 0;
        for i in 0..(MAX_ACTIVE_TOKENLESS_GRANTS + 10) {
            let resp = handle_initialize(
                &host,
                serde_json::json!(i),
                serde_json::json!({ "recipient": "loop" }),
            )
            .await;
            if resp.0.error.is_some() {
                refused += 1;
            }
        }
        assert_eq!(refused, 10, "calls past the tokenless cap are refused");
        assert_eq!(
            host.mcp.snapshot_grants().await.len(),
            MAX_ACTIVE_TOKENLESS_GRANTS,
            "the grant store stops growing at the cap"
        );
    }

    /// Mutation hardening: the recipient bound is inclusive, and revoked sessions stop
    /// counting toward the tokenless cap (so ending a session frees a slot).
    #[tokio::test]
    async fn pba_l7b_015_bounds_are_exact_and_revocation_frees_a_slot() {
        let host = test_host();
        let at_cap = "r".repeat(MAX_RECIPIENT_LEN);
        let resp = handle_initialize(
            &host,
            serde_json::json!(0),
            serde_json::json!({ "recipient": at_cap }),
        )
        .await;
        assert!(
            resp.0.error.is_none(),
            "a recipient exactly at the cap is accepted"
        );
        for i in 1..MAX_ACTIVE_TOKENLESS_GRANTS {
            let r = handle_initialize(&host, serde_json::json!(i), serde_json::json!({})).await;
            assert!(r.0.error.is_none());
        }
        let full = handle_initialize(&host, serde_json::json!(99), serde_json::json!({})).await;
        assert!(full.0.error.is_some(), "cap reached");
        let first = host.mcp.snapshot_grants().await[0].id.clone();
        assert!(host.mcp.revoke_grant(&first).await);
        let again = handle_initialize(&host, serde_json::json!(100), serde_json::json!({})).await;
        assert!(
            again.0.error.is_none(),
            "a revoked session no longer counts"
        );
    }

    #[test]
    fn pba_l7b_015_admission_rules() {
        assert!(admit_initialize(0, 0, true).is_ok());
        assert!(admit_initialize(0, MAX_ACTIVE_TOKENLESS_GRANTS - 1, true).is_ok());
        assert!(admit_initialize(0, MAX_ACTIVE_TOKENLESS_GRANTS, true).is_err());
        // An operator-token client is not limited by the tokenless cap ...
        assert!(admit_initialize(0, MAX_ACTIVE_TOKENLESS_GRANTS, false).is_ok());
        // ... but everyone is bounded by the total ceiling.
        assert!(admit_initialize(MAX_TOTAL_GRANTS - 1, 0, false).is_ok());
        assert!(admit_initialize(MAX_TOTAL_GRANTS, 0, false).is_err());
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
        let cfg = host
            .export_sidecar_config(Some("abc-123".to_string()))
            .await;
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
        assert!(
            body.error.is_none(),
            "no-token requests are accepted at ReadOnly"
        );
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
        let err = body.error.as_ref().expect("unknown auth_token must error");
        assert_eq!(err.code, -32001);
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

    // ── GUI_NATIVE-2026-05-31-005 (WP 6.4b): session/end ownership ──────

    /// A grant minted under an operator auth_token must only be endable by a
    /// caller presenting THAT token — knowing/guessing a grant_id alone must
    /// not let a co-resident client revoke someone else's session (grant DoS).
    #[tokio::test]
    async fn test_005_session_end_without_owner_token_is_refused() {
        let host = test_host();
        let token = host
            .create_token("owner", PolicyProfile::Guided, None)
            .await;
        let params = serde_json::json!({ "auth_token": token.token, "policy": "Guided" });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        let grant_id = resp.0.result.as_ref().expect("grant issued")["grant_id"]
            .as_str()
            .expect("grant_id string")
            .to_string();

        // No token presented → refused, grant survives.
        let end = handle_session_end(
            &host,
            serde_json::json!(2),
            serde_json::json!({ "grant_id": grant_id }),
        )
        .await;
        assert!(
            end.0.error.is_some(),
            "session/end without the owning auth_token must be refused"
        );
        assert_eq!(
            host.list_active_grants().await.len(),
            1,
            "the grant must survive an unowned session/end"
        );

        // Wrong token → refused too.
        let other = host
            .create_token("other", PolicyProfile::Guided, None)
            .await;
        let end2 = handle_session_end(
            &host,
            serde_json::json!(3),
            serde_json::json!({ "grant_id": resp.0.result.as_ref().unwrap()["grant_id"], "auth_token": other.token }),
        )
        .await;
        assert!(
            end2.0.error.is_some(),
            "a different token must not end the grant"
        );
        assert_eq!(host.list_active_grants().await.len(), 1);
    }

    /// Presenting the owning token ends the session.
    #[tokio::test]
    async fn test_005_session_end_with_owner_token_succeeds() {
        let host = test_host();
        let token = host
            .create_token("owner", PolicyProfile::Guided, None)
            .await;
        let params = serde_json::json!({ "auth_token": token.token, "policy": "Guided" });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        let grant_id = resp.0.result.as_ref().unwrap()["grant_id"].clone();

        let end = handle_session_end(
            &host,
            serde_json::json!(2),
            serde_json::json!({ "grant_id": grant_id, "auth_token": token.token }),
        )
        .await;
        assert!(
            end.0.error.is_none(),
            "owner-presented session/end succeeds"
        );
        assert_eq!(host.list_active_grants().await.len(), 0);
    }

    /// Tokenless (ReadOnly discovery) grants keep the v1 behavior: the
    /// unguessable grant_id itself is the credential.
    #[tokio::test]
    async fn test_005_tokenless_grant_can_end_with_grant_id_alone() {
        let host = test_host();
        let params = serde_json::json!({ "policy": "ReadOnly", "recipient": "hermes" });
        let resp = handle_initialize(&host, serde_json::json!(1), params).await;
        let grant_id = resp.0.result.as_ref().unwrap()["grant_id"].clone();

        let end = handle_session_end(
            &host,
            serde_json::json!(2),
            serde_json::json!({ "grant_id": grant_id }),
        )
        .await;
        assert!(
            end.0.error.is_none(),
            "tokenless grant ends with grant_id alone"
        );
        assert_eq!(host.list_active_grants().await.len(), 0);
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
        use_mock_keyring();
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(McpServer::new(registry.clone()));
        let host = Arc::new(McpHostService::with_token_storage(
            mcp,
            tmp.path().to_path_buf(),
        ));
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
        let host = storage_host(tmp.path());
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

    // ── ENCRYPT-S1 WP-4 (inventory A6): token vault ─────────────────

    /// Probe test (WP-4 AC): after a save, the on-disk file carries NO
    /// plaintext token bytes — not the secret, not the label — and is
    /// a sealed vault (magic prefix).
    #[tokio::test]
    async fn test_encrypt_s1_token_file_contains_no_plaintext_token_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let host = storage_host(tmp.path());
        let token = host
            .create_token("probe-label-hermes", PolicyProfile::Operator, None)
            .await;

        let path = tmp.path().join("mcp_tokens.json");
        let raw = std::fs::read(&path).expect("tokens file exists");
        assert!(
            raw.starts_with(TOKEN_VAULT_MAGIC),
            "tokens file must be a sealed vault"
        );
        let window_contains = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
        assert!(
            !window_contains(token.token.as_bytes()),
            "bearer token bytes must not appear on disk"
        );
        // Even a prefix of the secret must not leak in the clear.
        assert!(
            !window_contains(token.token[..16].as_bytes()),
            "token prefix must not appear on disk"
        );
        assert!(
            !window_contains(b"probe-label-hermes"),
            "token label must not appear on disk"
        );
        assert!(
            !window_contains(b"max_policy"),
            "JSON structure must not appear on disk"
        );
    }

    /// Legacy migration (WP-4 AC): a pre-ENCRYPT-S1 plaintext
    /// mcp_tokens.json is loaded, re-sealed in place, and the plaintext
    /// is gone; the token still validates after ANOTHER restart (i.e.
    /// the sealed file round-trips).
    #[tokio::test]
    async fn test_encrypt_s1_legacy_plaintext_file_migrates_to_ciphertext() {
        use_mock_keyring();
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mcp_tokens.json");

        // Fabricate the legacy plaintext format (a JSON array of AuthToken).
        let secret = "ab".repeat(32);
        let legacy = AuthToken {
            token: secret.clone(),
            max_policy: PolicyProfile::Guided,
            created_at: 1,
            expires_at: u64::MAX,
            label: "legacy-plaintext".to_string(),
            revoked: false,
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&vec![&legacy]).unwrap()).unwrap();

        // First load performs the migration.
        let host = storage_host(tmp.path());
        let summaries = host.list_tokens().await;
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].label, "legacy-plaintext");

        let raw = std::fs::read(&path).expect("file still exists (sealed)");
        assert!(
            raw.starts_with(TOKEN_VAULT_MAGIC),
            "file must now be sealed"
        );
        assert!(
            !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "plaintext token must be shredded from disk"
        );

        // The migrated token survives a second restart via the sealed file.
        let host2 = storage_host(tmp.path());
        let params = serde_json::json!({ "auth_token": secret, "policy": "Guided" });
        let resp = handle_initialize(&host2, serde_json::json!(1), params).await;
        assert!(resp.0.error.is_none(), "migrated token must still validate");
    }

    /// Vault primitives: seal/open round-trip, and tampering (or the
    /// wrong key) fails closed.
    #[test]
    fn test_encrypt_s1_vault_seal_open_roundtrip_and_tamper() {
        let key = [7u8; 32];
        let sealed = seal_token_blob(&key, b"[]").expect("seal");
        assert!(sealed.starts_with(TOKEN_VAULT_MAGIC));
        assert_eq!(open_token_blob(&key, &sealed).expect("open"), b"[]");

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 0x01;
        assert!(
            open_token_blob(&key, &tampered).is_err(),
            "tamper must fail"
        );

        let wrong = [8u8; 32];
        assert!(
            open_token_blob(&wrong, &sealed).is_err(),
            "wrong key must fail"
        );
    }

    // ── NAT-B-024: AEAD associated data binds the vault magic ────────────
    #[test]
    fn test_natb024_vault_aad_binds_magic() {
        use aes_gcm::aead::{Aead, Payload};
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        let key = [9u8; 32];
        let sealed = seal_token_blob(&key, b"[]").expect("seal");
        let body = &sealed[TOKEN_VAULT_MAGIC.len()..];
        let (nonce, ct) = body.split_at(TOKEN_VAULT_NONCE_LEN);
        let cipher = Aes256Gcm::new((&key).into());
        // Pre-fix behavior (empty AAD) must NO LONGER open the blob.
        assert!(cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: b"" })
            .is_err());
        // A swapped magic (downgrade) also fails to authenticate.
        assert!(cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: b"CITMCPV2"
                }
            )
            .is_err());
        // Only the bound magic authenticates.
        assert!(cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: TOKEN_VAULT_MAGIC.as_slice()
                }
            )
            .is_ok());
    }

    // ── NAT-B-023: constant-time compare + revoke min-length ─────────────
    #[test]
    fn test_natb023_ct_eq() {
        assert!(ct_eq(b"abcd", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    #[tokio::test]
    async fn test_natb023_revoke_rejects_empty_and_short_prefix() {
        let host = test_host();
        let a = host.create_token("a", PolicyProfile::Operator, None).await;
        let _b = host.create_token("b", PolicyProfile::Operator, None).await;
        // Empty / short arguments must NOT revoke an arbitrary token.
        assert!(!host.revoke_token("").await, "empty must not revoke");
        assert!(!host.revoke_token("a").await, "1-char must not revoke");
        assert!(
            !host.revoke_token("abcdefg").await,
            "7-char must not revoke"
        );
        // A full token still revokes exactly itself.
        assert!(host.revoke_token(&a.token).await, "full token revokes");
    }

    // ── NAT-B-011: DNS-rebinding guard + tools/list expiry ───────────────
    #[test]
    fn test_natb011_is_local_request() {
        // Native client: no Origin, loopback Host → allowed.
        assert!(is_local_request(None, Some("127.0.0.1:9600")));
        assert!(is_local_request(None, Some("localhost:9600")));
        assert!(is_local_request(None, Some("[::1]:9600")));
        assert!(is_local_request(None, None));
        // Any Origin (browser/DNS-rebinding page) → refused.
        assert!(!is_local_request(
            Some("http://evil.example"),
            Some("127.0.0.1:9600")
        ));
        // Rebound hostname in Host → refused.
        assert!(!is_local_request(None, Some("evil.example:9600")));
        assert!(!is_local_request(None, Some("attacker.tld")));
    }

    #[test]
    fn test_natb011_grant_is_expired() {
        // now = 1000; a grant expiring at 500 is expired, at 2000 is live.
        let past = chrono::DateTime::<chrono::Utc>::from_timestamp(500, 0)
            .unwrap()
            .to_rfc3339();
        let future = chrono::DateTime::<chrono::Utc>::from_timestamp(2000, 0)
            .unwrap()
            .to_rfc3339();
        assert!(grant_is_expired(&past, 1000));
        assert!(!grant_is_expired(&future, 1000));
        // Unparseable → fail closed (expired).
        assert!(grant_is_expired("not-a-date", 1000));
    }

    #[tokio::test]
    async fn test_natb011_tools_list_refuses_expired_grant() {
        let host = test_host();
        // Mint a grant via initialize, then forge an expired grant_id by
        // driving tools/list against a grant whose expiry is in the past.
        // Prove an EXPIRED but present grant is refused by tools/list by
        // pushing one directly (CapabilityGrant is in scope via super::*).
        let expired = CapabilityGrant {
            id: "expired-grant".to_string(),
            issuer: "citrate-gui".to_string(),
            recipient: "t".to_string(),
            allowed_tools: vec![],
            max_value_per_tx: None,
            allowed_paths: vec![],
            expires_at: chrono::DateTime::<chrono::Utc>::from_timestamp(1, 0)
                .unwrap()
                .to_rfc3339(),
            policy: PolicyProfile::ReadOnly,
            revoked: false,
            connected_since: 0,
            issuer_pubkey: vec![],
            signature: vec![],
        };
        host.mcp.add_grant(expired).await;
        let params = serde_json::json!({ "grant_id": "expired-grant" });
        let resp = handle_tools_list(&host, serde_json::json!(1), params).await;
        assert!(
            resp.0.error.is_some(),
            "expired grant must be refused by tools/list"
        );
    }
}
