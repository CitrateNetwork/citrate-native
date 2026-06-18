//! EW-S1 WP-8 — "Link this device": bind this wallet's EOA to the
//! user's Citrate identity smart wallet, and send paymaster-sponsored
//! UserOperations from it.
//!
//! Flow (mirrors the wallet-extension's WP-9 implementation):
//!   1. RFC-8252 loopback OIDC (Authorization Code + PKCE S256) against
//!      `auth.citrate.ai` as client `citrate-gui-native`.
//!   2. `sub` → 32-byte AA userId (see `citrate_aa`) → CREATE2 wallet
//!      prediction, cross-checked against the authority's
//!      `wallet_address` claim.
//!   3. Wallet not deployed → `POST /aa/enroll-validator` (Bearer) for a
//!      deploy permit, then `factory.deployFor` sent from this EOA via
//!      the wallet service, installing the EOA as the wallet's
//!      `CitrateECDSAValidator` root (source = gui-native).
//!      Already deployed (passkey root) → surface the dashboard
//!      approval path instead of failing silently.
//!   4. Sponsored sends: root-key nonce from the EntryPoint, ERC-7579
//!      `execute(single)` calldata, recoverable secp256k1 over the v0.7
//!      userOpHash, submitted to `bundler.citrate.ai`.
//!
//! Data sources: auth.citrate.ai (`/auth`, `/token`,
//! `/aa/enroll-validator`); the chain RPC (`eth_getCode`, `eth_call`
//! EntryPoint.getNonce, `eth_gasPrice`); bundler.citrate.ai
//! (`eth_sendUserOperation`). Canonical addresses: `citrate_aa::addresses`.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as Sha2Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::AppError;

use super::citrate_aa::{self, addresses};
use super::wallet_service::WalletService;

const DEFAULT_AUTH_URL: &str = "https://auth.citrate.ai";
const DEFAULT_BUNDLER_URL: &str = "https://bundler.citrate.ai/rpc";
const OIDC_CLIENT_ID: &str = "citrate-gui-native";

/// Persisted link state (JSON next to the app config — holds NO secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CitrateLink {
    pub sub: String,
    pub user_id_hex: String,
    pub smart_wallet: String,
    pub eoa: String,
    pub deployed: bool,
    /// Wallet already existed with another root signer (passkey) — this
    /// EOA still needs root-signer approval from the dashboard.
    pub pending_root_enroll: bool,
    pub linked_at: String,
    /// AUTHSPINE S3-WP3: a snapshot of the entitlement/KYC claims from the
    /// id_token at link time — display only. `#[serde(default)]` so link
    /// files written before this field still deserialize.
    #[serde(default)]
    pub kyc_status: String,
    /// Effective access tier ("public" when no entitlement claim was present).
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Citrate role from the entitlement claim, if any.
    #[serde(default)]
    pub citrate_role: Option<String>,
}

fn default_tier() -> String {
    "public".to_string()
}

/// The shared access-entitlement claim every Citrate RP reads — the same
/// `https://citrate.ai/entitlement` contract as `@citrate/oidc-client` and the
/// other native apps (studio). Carried on the `openid` scope; absent ⇒ Public.
const ENTITLEMENT_CLAIM: &str = "https://citrate.ai/entitlement";

/// Centralized RBAC entitlement (tier + optional role) parsed from the claim.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entitlement {
    pub tier: String,
    pub citrate_role: Option<String>,
    pub expires_at: Option<i64>,
}

/// Tier ladder rank, matching the TS `TIER_ORDER`. Unknown/absent ⇒ public (0).
fn tier_rank(tier: &str) -> u8 {
    match tier {
        "commercial" => 1,
        "commercial.kyc" => 2,
        "academic" => 3,
        "confidential" => 4,
        _ => 0,
    }
}

/// Parse the entitlement claim object; unknown/absent tier ⇒ None (Public,
/// fail-safe) — identical to the TS `parseEntitlement` contract.
fn parse_entitlement(v: &serde_json::Value) -> Option<Entitlement> {
    let o = v.as_object()?;
    let tier = o.get("tier").and_then(|x| x.as_str())?;
    if tier_rank(tier) == 0 && tier != "public" {
        return None;
    }
    Some(Entitlement {
        tier: tier.to_string(),
        citrate_role: o.get("citrateRole").and_then(|x| x.as_str()).map(|s| s.to_string()),
        expires_at: o.get("expiresAt").and_then(|x| x.as_i64()),
    })
}

impl Entitlement {
    /// Effective tier honoring expiry (expired ⇒ public). `now_unix` in seconds.
    pub fn effective_tier(&self, now_unix: i64) -> &str {
        match self.expires_at {
            Some(exp) if now_unix > exp => "public",
            _ => &self.tier,
        }
    }
}

/// The hosted Account Hub URL on the issuer ("Manage account / Upgrade") — the
/// ecosystem-wide entry point to complete or upgrade KYC. Mirrors the TS
/// `accountHubUrl`.
pub fn account_hub_url(auth_url: &str, return_to: Option<&str>) -> String {
    let base = format!("{}/account", auth_url.trim_end_matches('/'));
    match return_to {
        Some(r) => format!("{}?return_to={}", base, urlencode(r)),
        None => base,
    }
}

pub struct CitrateLinkService {
    wallet: Arc<WalletService>,
    http: reqwest::Client,
    auth_url: String,
    bundler_url: String,
    link_path: PathBuf,
}

impl CitrateLinkService {
    pub fn new(wallet: Arc<WalletService>, link_path: PathBuf) -> Self {
        Self {
            wallet,
            http: reqwest::Client::new(),
            auth_url: std::env::var("CITRATE_AUTH_URL").unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string()),
            bundler_url: std::env::var("CITRATE_BUNDLER_URL").unwrap_or_else(|_| DEFAULT_BUNDLER_URL.to_string()),
            link_path,
        }
    }

    /// The persisted link, if this device has one.
    pub fn load_link(&self) -> Option<CitrateLink> {
        let raw = std::fs::read_to_string(&self.link_path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn save_link(&self, link: &CitrateLink) -> Result<(), AppError> {
        if let Some(parent) = self.link_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::Wallet(format!("cannot create config dir: {}", e)))?;
        }
        let json = serde_json::to_string_pretty(link)
            .map_err(|e| AppError::Wallet(format!("link serialize failed: {}", e)))?;
        std::fs::write(&self.link_path, json)
            .map_err(|e| AppError::Wallet(format!("link persist failed: {}", e)))?;
        Ok(())
    }

    /// Run the full link flow. `eoa` is this device's selected account.
    pub async fn link(&self, eoa: &str) -> Result<CitrateLink, AppError> {
        // Gate: the EOA must be secp256k1 — an Ed25519 key can never own
        // the wallet's CitrateECDSAValidator.
        let kind = self.wallet.key_kind(eoa).await?;
        if kind != "secp256k1" {
            return Err(AppError::Wallet(format!(
                "This account uses an {} key; linking needs a secp256k1 account (import one from a mnemonic or key)",
                kind
            )));
        }

        let login = self.oidc_login().await?;
        let sub = login.sub.clone();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let tier = login
            .entitlement
            .as_ref()
            .map(|e| e.effective_tier(now_unix).to_string())
            .unwrap_or_else(default_tier);
        let citrate_role = login.entitlement.as_ref().and_then(|e| e.citrate_role.clone());
        let kyc_status = login.kyc_status.clone().unwrap_or_default();

        let user_id = citrate_aa::account_id_to_user_id(&sub)
            .map_err(|e| AppError::Wallet(format!("cannot derive AA userId from subject {}: {}", sub, e)))?;
        let smart_wallet =
            citrate_aa::predict_wallet_address(addresses::FACTORY, addresses::WALLET_IMPL, &user_id)
                .map_err(|e| AppError::Wallet(format!("address prediction failed: {}", e)))?;

        // Cross-check the authority's own view of the address.
        if let Some(claimed) = &login.wallet_address {
            if claimed.to_lowercase() != smart_wallet {
                return Err(AppError::Wallet(format!(
                    "address mismatch: authority says {}, local prediction {}",
                    claimed, smart_wallet
                )));
            }
        }

        let deployed_code = self
            .rpc_call(&self.wallet.get_rpc_url(), "eth_getCode", serde_json::json!([smart_wallet, "latest"]))
            .await?;
        let already_deployed = deployed_code.as_str().is_some_and(|c| c != "0x" && c != "0x0");

        let mut pending_root_enroll = false;
        let mut deployed = already_deployed;
        if !already_deployed {
            self.deploy_via_permit(&login.access_token, &user_id, eoa, &smart_wallet)
                .await?;
            deployed = true;
        } else {
            pending_root_enroll = true;
        }

        let link = CitrateLink {
            sub,
            user_id_hex: format!("0x{}", hex::encode(user_id)),
            smart_wallet,
            eoa: eoa.to_lowercase(),
            deployed,
            pending_root_enroll,
            linked_at: chrono_like_now(),
            kyc_status,
            tier,
            citrate_role,
        };
        self.save_link(&link)?;
        Ok(link)
    }

    /// Fetch a deploy permit and send `deployFor` from the EOA.
    async fn deploy_via_permit(
        &self,
        access_token: &str,
        user_id: &[u8; 32],
        eoa: &str,
        expected_wallet: &str,
    ) -> Result<String, AppError> {
        let install = citrate_aa::ecdsa_install_data(eoa, citrate_aa::ECDSA_SOURCE_GUI_NATIVE)
            .map_err(|e| AppError::Wallet(format!("install data: {}", e)))?;
        let init_data = citrate_aa::encode_initialize(addresses::ECDSA_VALIDATOR, &install)
            .map_err(|e| AppError::Wallet(format!("initialize calldata: {}", e)))?;
        let expires_at = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| AppError::Wallet(format!("clock error: {}", e)))?
            .as_secs())
            + 3600;

        #[derive(Deserialize)]
        struct EnrollResponse {
            signature: Option<String>,
            #[serde(rename = "predictedAddress")]
            predicted_address: Option<String>,
            reason: Option<String>,
            error: Option<String>,
        }
        let resp = self
            .http
            .post(format!("{}/aa/enroll-validator", self.auth_url))
            .bearer_auth(access_token)
            .json(&serde_json::json!({
                "userId": format!("0x{}", hex::encode(user_id)),
                "initData": format!("0x{}", hex::encode(&init_data)),
                "expiresAt": expires_at,
            }))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("enroll-validator unreachable: {}", e)))?;
        let status = resp.status();
        let enroll: EnrollResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Network(format!("enroll-validator bad response: {}", e)))?;
        let signature_hex = match enroll.signature {
            Some(s) if status.is_success() => s,
            _ => {
                return Err(AppError::Wallet(format!(
                    "enroll-validator failed: {}",
                    enroll.reason.or(enroll.error).unwrap_or_else(|| status.to_string())
                )))
            }
        };
        // The permit's own prediction must agree with what we computed —
        // a divergence means the authority would deploy a different wallet.
        if let Some(predicted) = &enroll.predicted_address {
            if predicted.to_lowercase() != expected_wallet {
                return Err(AppError::Wallet(
                    "authority permit predicts a different wallet address — aborting".to_string(),
                ));
            }
        }
        let signature = hex::decode(signature_hex.trim_start_matches("0x"))
            .map_err(|e| AppError::Wallet(format!("permit signature not hex: {}", e)))?;

        let calldata = citrate_aa::encode_deploy_for(
            user_id,
            addresses::ECDSA_VALIDATOR,
            &init_data,
            expires_at,
            &signature,
        )
        .map_err(|e| AppError::Wallet(format!("deployFor calldata: {}", e)))?;

        // Session-gated legacy tx from the EOA (it pays its own gas here;
        // the SPONSORED path begins once the wallet exists).
        self.wallet
            .send_transaction_with_data(eoa, addresses::FACTORY, "0", calldata, "")
            .await
    }

    /// Send a paymaster-sponsored UserOperation from the linked smart
    /// wallet. Returns the userOpHash the bundler accepted.
    pub async fn send_sponsored(
        &self,
        to: &str,
        value_wei: u128,
        data: Vec<u8>,
    ) -> Result<String, AppError> {
        let link = self
            .load_link()
            .ok_or_else(|| AppError::Wallet("No Citrate smart wallet linked yet".to_string()))?;
        if link.pending_root_enroll {
            return Err(AppError::Wallet(
                "This device's key is not yet a validator on the smart wallet — finish linking from the auth.citrate.ai dashboard".to_string(),
            ));
        }
        let rpc_url = self.wallet.get_rpc_url();

        // Root-validator nonce (key 0) straight from the EntryPoint.
        let nonce_calldata = citrate_aa::get_nonce_calldata(&link.smart_wallet)
            .map_err(|e| AppError::Wallet(format!("getNonce calldata: {}", e)))?;
        let nonce_hex = self
            .rpc_call(
                &rpc_url,
                "eth_call",
                serde_json::json!([{ "to": addresses::ENTRY_POINT, "data": format!("0x{}", hex::encode(nonce_calldata)) }, "latest"]),
            )
            .await?;
        let nonce = parse_quantity(&nonce_hex)?;

        let gas_price_hex = self.rpc_call(&rpc_url, "eth_gasPrice", serde_json::json!([])).await?;
        let gas_price = parse_quantity(&gas_price_hex)?;

        let call_data = citrate_aa::encode_execute_single(to, value_wei, &data)
            .map_err(|e| AppError::Wallet(format!("execute calldata: {}", e)))?;
        let paymaster_and_data =
            citrate_aa::pack_paymaster_and_data(addresses::PAYMASTER, 80_000, 60_000, 0)
                .map_err(|e| AppError::Wallet(format!("paymaster data: {}", e)))?;

        let op = citrate_aa::PackedUserOp {
            sender: &link.smart_wallet,
            nonce,
            init_code: &[],
            call_data: &call_data,
            account_gas_limits: citrate_aa::pack_pair128(300_000, 200_000),
            pre_verification_gas: 60_000,
            gas_fees: citrate_aa::pack_pair128(gas_price, gas_price.saturating_mul(2)),
            paymaster_and_data: &paymaster_and_data,
        };
        let digest = citrate_aa::get_user_op_hash(&op, addresses::ENTRY_POINT, addresses::CHAIN_ID)
            .map_err(|e| AppError::Wallet(format!("userOpHash: {}", e)))?;

        let signature = self.wallet.sign_digest_recoverable(&link.eoa, digest).await?;

        // Unpacked v0.7 wire shape for eth-infinitism's bundler.
        let wire = serde_json::json!({
            "sender": link.smart_wallet,
            "nonce": format!("0x{:x}", nonce),
            "callData": format!("0x{}", hex::encode(&call_data)),
            "callGasLimit": "0x30d40",            // 200000
            "verificationGasLimit": "0x493e0",    // 300000
            "preVerificationGas": "0xea60",       // 60000
            "maxPriorityFeePerGas": format!("0x{:x}", gas_price),
            "maxFeePerGas": format!("0x{:x}", gas_price.saturating_mul(2)),
            "paymaster": addresses::PAYMASTER,
            "paymasterVerificationGasLimit": "0x13880", // 80000
            "paymasterPostOpGasLimit": "0xea60",        // 60000
            "paymasterData": "0x00",                    // standard category
            "signature": format!("0x{}", hex::encode(signature)),
        });
        let body: serde_json::Value = self
            .http
            .post(&self.bundler_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "eth_sendUserOperation",
                "params": [wire, addresses::ENTRY_POINT],
            }))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("bundler unreachable: {}", e)))?
            .json()
            .await
            .map_err(|e| AppError::Network(format!("bundler bad response: {}", e)))?;
        if let Some(err) = body.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
            return Err(AppError::Network(format!("bundler rejected the UserOperation: {}", msg)));
        }
        body.get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AppError::Network("bundler returned no userOpHash".to_string()))
    }

    // ── OIDC loopback (RFC 8252) ────────────────────────────────────

    async fn oidc_login(&self) -> Result<OidcLogin, AppError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| AppError::Network(format!("cannot bind loopback listener: {}", e)))?;
        let port = listener
            .local_addr()
            .map_err(|e| AppError::Network(format!("loopback addr: {}", e)))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{}/auth/callback", port);

        let verifier = random_b64url(32);
        let challenge = b64url_encode(&Sha256::digest(verifier.as_bytes()));
        let state = random_b64url(16);

        let auth_url = format!(
            "{}/auth?client_id={}&redirect_uri={}&response_type=code&scope=openid%20wallet&state={}&code_challenge={}&code_challenge_method=S256",
            self.auth_url,
            OIDC_CLIENT_ID,
            urlencode(&redirect_uri),
            state,
            challenge,
        );
        open_in_browser(&auth_url)?;

        let (code, returned_state) = accept_oidc_callback(listener).await?;
        if returned_state != state {
            return Err(AppError::Wallet("OIDC state mismatch — aborting (possible CSRF)".to_string()));
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            id_token: Option<String>,
            error: Option<String>,
            error_description: Option<String>,
        }
        let tokens: TokenResponse = self
            .http
            .post(format!("{}/token", self.auth_url))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", OIDC_CLIENT_ID),
                ("code_verifier", verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|e| AppError::Network(format!("token endpoint unreachable: {}", e)))?
            .json()
            .await
            .map_err(|e| AppError::Network(format!("token endpoint bad response: {}", e)))?;

        let (access_token, id_token) = match (tokens.access_token, tokens.id_token) {
            (Some(a), Some(i)) => (a, i),
            _ => {
                return Err(AppError::Wallet(format!(
                    "token exchange failed: {}",
                    tokens.error_description.or(tokens.error).unwrap_or_else(|| "unknown".to_string())
                )))
            }
        };

        // Payload decode WITHOUT signature verification — acceptable only
        // because the token arrives directly from the authority's /token
        // endpoint over TLS in the code+PKCE exchange (we are the client).
        let claims = decode_jwt_payload(&id_token)?;
        let sub = claims
            .get("sub")
            .and_then(|s| s.as_str())
            .ok_or_else(|| AppError::Wallet("id_token carries no sub".to_string()))?
            .to_string();
        let wallet_address = claims
            .get("wallet_address")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let kyc_status = claims
            .get("kyc_status")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let entitlement = claims.get(ENTITLEMENT_CLAIM).and_then(parse_entitlement);

        Ok(OidcLogin { access_token, sub, wallet_address, kyc_status, entitlement })
    }

    /// Open the hosted Account Hub (KYC / tier upgrade) in the system browser —
    /// the ecosystem-wide entry point to complete or raise the account tier.
    pub fn open_account_hub(&self) -> Result<(), AppError> {
        open_in_browser(&account_hub_url(&self.auth_url, Some("https://citrate.ai")))
    }

    async fn rpc_call(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AppError> {
        let body: serde_json::Value = self
            .http
            .post(url)
            .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .map_err(|e| AppError::Network(format!("{} unreachable: {}", method, e)))?
            .json()
            .await
            .map_err(|e| AppError::Network(format!("{} bad response: {}", method, e)))?;
        if let Some(err) = body.get("error") {
            return Err(AppError::Network(format!("{} failed: {}", method, err)));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| AppError::Network(format!("{} returned no result", method)))
    }
}

struct OidcLogin {
    access_token: String,
    sub: String,
    wallet_address: Option<String>,
    kyc_status: Option<String>,
    entitlement: Option<Entitlement>,
}

/// Wait for the single authorization-code redirect on the loopback
/// listener; parse `code` + `state` and answer with a tiny HTML page.
async fn accept_oidc_callback(listener: TcpListener) -> Result<(String, String), AppError> {
    // The browser may probe with favicon requests etc. — accept until we
    // see the /auth/callback GET, bounded to a handful of connections.
    for _ in 0..8 {
        let (mut stream, _) = tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| AppError::Network("sign-in timed out (5 minutes)".to_string()))?
            .map_err(|e| AppError::Network(format!("loopback accept: {}", e)))?;
        let mut buf = vec![0u8; 8192];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| AppError::Network(format!("loopback read: {}", e)))?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or_default();

        if let Some(query) = first_line
            .strip_prefix("GET /auth/callback?")
            .and_then(|rest| rest.split(' ').next())
        {
            let mut code = None;
            let mut state = None;
            for pair in query.split('&') {
                let mut kv = pair.splitn(2, '=');
                match (kv.next(), kv.next()) {
                    (Some("code"), Some(v)) => code = Some(urldecode(v)),
                    (Some("state"), Some(v)) => state = Some(urldecode(v)),
                    _ => {}
                }
            }
            let page = "<html><body style=\"font-family:sans-serif;text-align:center;padding-top:4rem\">\
                        <h2>Linked to Citrate</h2><p>You can close this tab and return to Citrate Native.</p>\
                        </body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                page.len(),
                page
            );
            let _ = stream.write_all(response.as_bytes()).await;
            match (code, state) {
                (Some(c), Some(s)) => return Ok((c, s)),
                _ => return Err(AppError::Wallet("authority redirected without a code".to_string())),
            }
        }
        // Not the callback (favicon, probe) — 404 it and keep listening.
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
    }
    Err(AppError::Network("no OIDC callback received".to_string()))
}

// ── small pure helpers (unit-tested below) ──────────────────────────

fn b64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(triple >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[triple as usize & 0x3f] as char);
        }
    }
    out
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, AppError> {
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return Err(AppError::Wallet(format!("invalid base64url byte {}", c))),
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            return Err(AppError::Wallet("truncated base64url".to_string()));
        }
        out.push((chunk[0] << 2) | (chunk[1] >> 4));
        if chunk.len() > 2 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    Ok(out)
}

fn decode_jwt_payload(jwt: &str) -> Result<serde_json::Value, AppError> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| AppError::Wallet("malformed JWT".to_string()))?;
    let bytes = b64url_decode(payload)?;
    serde_json::from_slice(&bytes).map_err(|e| AppError::Wallet(format!("JWT payload not JSON: {}", e)))
}

fn random_b64url(len: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url_encode(&bytes)
}

fn parse_quantity(v: &serde_json::Value) -> Result<u128, AppError> {
    let s = v
        .as_str()
        .ok_or_else(|| AppError::Network(format!("expected hex quantity, got {}", v)))?;
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| AppError::Network(format!("bad hex quantity {}: {}", s, e)))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hexpair = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hexpair, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ISO-8601 UTC timestamp without pulling in chrono.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days-from-civil inverse (Howard Hinnant's algorithm).
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mth, d, h, m, s)
}

fn open_in_browser(url: &str) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let cmd = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let cmd = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    cmd.map(|_| ())
        .map_err(|e| AppError::Wallet(format!("cannot open the browser for sign-in: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_round_trips_and_matches_known_vectors() {
        // RFC 4648 test vector ("foobar" family), URL-safe alphabet.
        assert_eq!(b64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(b64url_encode(b"foobar"), "Zm9vYmFy");
        let bytes: Vec<u8> = (0u8..=255).collect();
        let enc = b64url_encode(&bytes);
        assert_eq!(b64url_decode(&enc).expect("round trip"), bytes);
        assert!(enc.bytes().all(|c| c != b'+' && c != b'/' && c != b'='));
    }

    #[test]
    fn jwt_payload_decodes() {
        let payload = serde_json::json!({
            "sub": "0d1f02f1-1f5a-4f5e-9c2e-7b8d1a2b3c4d",
            "wallet_address": "0x05d25D894E88B288f3F7508ce6523D79DEE5DE28"
        });
        let body = b64url_encode(payload.to_string().as_bytes());
        let jwt = format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", body);
        let claims = decode_jwt_payload(&jwt).expect("payload decodes");
        assert_eq!(
            claims.get("sub").and_then(|s| s.as_str()),
            Some("0d1f02f1-1f5a-4f5e-9c2e-7b8d1a2b3c4d")
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        // RFC 7636 Appendix B: verifier → S256 challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = b64url_encode(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn url_helpers_round_trip() {
        let uri = "http://127.0.0.1:7777/auth/callback";
        assert_eq!(urlencode(uri), "http%3A%2F%2F127.0.0.1%3A7777%2Fauth%2Fcallback");
        assert_eq!(urldecode(&urlencode(uri)), uri);
        assert_eq!(urldecode("a%2Bb+c"), "a+b c");
    }

    #[test]
    fn link_persistence_round_trips() {
        let dir = std::env::temp_dir().join(format!("citrate-link-test-{}", std::process::id()));
        let path = dir.join("citrate_link.json");
        let link = CitrateLink {
            sub: "0d1f02f1-1f5a-4f5e-9c2e-7b8d1a2b3c4d".to_string(),
            user_id_hex: "0x23691dc9a1d9d7ffa4787edf129321063826c584f406598e645141dba9db32d8".to_string(),
            smart_wallet: "0x05d25d894e88b288f3f7508ce6523d79dee5de28".to_string(),
            eoa: "0x8ba1f109551bd432803012645ac136ddd64dba72".to_string(),
            deployed: true,
            pending_root_enroll: false,
            linked_at: chrono_like_now(),
            kyc_status: "verified".to_string(),
            tier: "commercial.kyc".to_string(),
            citrate_role: Some("operator".to_string()),
        };
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(&path, serde_json::to_string(&link).expect("serialize")).expect("write");
        let raw = std::fs::read_to_string(&path).expect("read");
        let loaded: CitrateLink = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(loaded.smart_wallet, link.smart_wallet);
        assert_eq!(loaded.tier, "commercial.kyc");
        assert_eq!(loaded.kyc_status, "verified");
        assert!(loaded.linked_at.ends_with('Z'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_link_json_without_entitlement_fields_still_loads() {
        // AUTHSPINE S3-WP3: link files written before the entitlement fields must
        // still deserialize (serde defaults: tier→public, kyc→"", role→None).
        let raw = r#"{"sub":"s","user_id_hex":"0x00","smart_wallet":"0xabc","eoa":"0xdef","deployed":true,"pending_root_enroll":false,"linked_at":"2026-01-01T00:00:00Z"}"#;
        let loaded: CitrateLink = serde_json::from_str(raw).expect("legacy link loads");
        assert_eq!(loaded.tier, "public");
        assert_eq!(loaded.kyc_status, "");
        assert_eq!(loaded.citrate_role, None);
    }

    #[test]
    fn entitlement_parse_tier_rank_and_expiry() {
        // unknown tier ⇒ None (public, fail-safe); known tier parses with role.
        assert!(parse_entitlement(&serde_json::json!({"tier":"superadmin"})).is_none());
        let e = parse_entitlement(&serde_json::json!({
            "tier":"confidential","orgId":"citrate","citrateRole":"auditor"
        }))
        .expect("parses");
        assert_eq!(e.tier, "confidential");
        assert_eq!(e.citrate_role.as_deref(), Some("auditor"));
        // ladder + expiry collapse.
        assert!(tier_rank("commercial.kyc") > tier_rank("commercial"));
        assert_eq!(tier_rank("public"), 0);
        let expired = Entitlement { tier: "academic".into(), expires_at: Some(50), citrate_role: None };
        assert_eq!(expired.effective_tier(100), "public");
        assert_eq!(expired.effective_tier(10), "academic");
    }

    #[test]
    fn account_hub_url_builds() {
        assert_eq!(account_hub_url("https://auth.citrate.ai", None), "https://auth.citrate.ai/account");
        assert_eq!(
            account_hub_url("https://auth.citrate.ai/", Some("https://citrate.ai")),
            "https://auth.citrate.ai/account?return_to=https%3A%2F%2Fcitrate.ai"
        );
    }

    #[tokio::test]
    async fn loopback_callback_parses_code_and_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let client = tokio::spawn(async move {
            // A favicon probe first (must be ignored), then the real callback.
            let mut s1 = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            s1.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await.expect("write");
            let mut sink = Vec::new();
            let _ = s1.read_to_end(&mut sink).await;

            let mut s2 = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            s2.write_all(b"GET /auth/callback?code=abc%2B123&state=st_42 HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
            let mut page = Vec::new();
            let _ = s2.read_to_end(&mut page).await;
            String::from_utf8_lossy(&page).into_owned()
        });

        let (code, state) = accept_oidc_callback(listener).await.expect("callback parsed");
        assert_eq!(code, "abc+123");
        assert_eq!(state, "st_42");
        let page = client.await.expect("client task");
        assert!(page.contains("return to Citrate Native"));
    }
}
