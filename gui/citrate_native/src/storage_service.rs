//! Storage service — IPFS-backed file uploads + a local `files.json`
//! index so the UI can show friendly names, sizes, and dates without
//! re-querying the IPFS daemon for metadata.
//!
//! P960-C WP-C.1 / C.3 / C.4 live here.
//!
//! ## Data flow
//!
//! 1. User clicks Upload (or drops a file) → Rust spawns an async
//!    task that POSTs `/api/v0/add?pin=true` to the local IPFS HTTP
//!    gateway (`127.0.0.1:5001`).
//! 2. IPFS returns `{"Hash": "Qm...", "Size": "12345", "Name": "file.pdf"}`.
//! 3. We persist a `FileRecord` to `~/.local/share/citrate-gui/files.json`
//!    — this carries the user-facing name and any future metadata
//!    (tags, description, rename).
//! 4. UI polls `list_files()` or the upload handler emits a direct
//!    push to the Slint model.
//!
//! ## Why a local index
//!
//! The IPFS daemon knows CIDs and raw sizes. It does NOT know the
//! user's intent ("this PDF is my resume, mime=application/pdf,
//! uploaded at 3pm Tuesday"). Keeping that metadata in our own index
//! lets the file-list view be human-readable without repeated HTTP
//! hits, and lets rename/tag persist across daemon restarts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One row in `files.json`. Mirrors the Slint `FileEntry` struct
/// minus the pre-formatted display fields (which we compute fresh
/// at render time so they stay accurate as time passes).
///
/// ENCRYPT-S1 WP-9a: the three envelope fields are serde-defaulted so
/// a pre-encryption `files.json` loads unchanged (`encrypted` reads as
/// `false`, wrap material empty), and older builds reading a newer
/// index simply ignore the extra fields (serde's default posture).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileRecord {
    /// CID returned by IPFS `add`. This is the stable identifier
    /// across renames. For an encrypted upload it addresses the
    /// CIPHERTEXT — anyone can fetch it, only the key holder can read.
    pub cid: String,
    /// User-facing name. Defaults to the original filename; can be
    /// changed via rename without affecting the CID.
    pub name: String,
    /// Bytes, raw. Pre-formatting happens in the UI layer. For
    /// encrypted uploads this is the PLAINTEXT size (what the user
    /// recognizes); ciphertext is +16 bytes of AEAD tag.
    pub size_bytes: u64,
    /// Unix seconds at upload time.
    pub uploaded_at: u64,
    /// MIME type guessed from the filename extension at upload.
    /// Stable across renames because the extension is captured then.
    pub mime: String,
    /// True when the blob behind `cid` is a client-side-encrypted
    /// envelope (encrypt-before-add). See ADR-2026-07-04-ipfs-client-
    /// side-encryption in citrate-federation.
    #[serde(default)]
    pub encrypted: bool,
    /// hex(ephemeral_x25519_pub(32) ‖ chacha20poly1305_wrapped_file_key(48)).
    /// Empty for plaintext uploads. Useless without the owner's
    /// keyring-held X25519 secret — safe-ish at rest, but files.json's
    /// own at-rest encryption rides ENCRYPT-S1 WP-1.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wrapped_key: String,
    /// hex of the 12-byte ChaCha20-Poly1305 nonce used on the file
    /// content. Empty for plaintext uploads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nonce: String,
}

impl FileRecord {
    /// One-glyph icon based on the major mime type. Chosen so every
    /// row has a consistent visual anchor even before the user reads
    /// the name.
    pub fn mime_icon(&self) -> &'static str {
        let major = self.mime.split('/').next().unwrap_or("");
        match major {
            "image" => "🖼",
            "video" => "🎬",
            "audio" => "🎵",
            "text" => "📄",
            "application" => match self.mime.as_str() {
                "application/pdf" => "📕",
                "application/zip"
                | "application/x-tar"
                | "application/gzip"
                | "application/x-7z-compressed" => "🗜",
                "application/json" => "📑",
                _ => "📦",
            },
            _ => "📁",
        }
    }

    /// Human-readable relative time (≤ 60s → "just now", ≤ 60m → "Xm ago",
    /// ≤ 48h → "Xh ago", else ISO date). Keeps the file list readable
    /// without pulling in `chrono` for such a narrow need.
    pub fn uploaded_display(&self, now_secs: u64) -> String {
        if self.uploaded_at == 0 || now_secs == 0 || self.uploaded_at > now_secs {
            return "—".to_string();
        }
        let diff = now_secs.saturating_sub(self.uploaded_at);
        if diff < 60 {
            "just now".to_string()
        } else if diff < 3600 {
            format!("{}m ago", diff / 60)
        } else if diff < 172_800 {
            format!("{}h ago", diff / 3600)
        } else {
            // Very rough date format — days since epoch ÷ year. For a
            // file-list view "absolute date" this is acceptable; if
            // users complain we can pull in `chrono`.
            let days = diff / 86_400;
            format!("{} days ago", days)
        }
    }

    /// Pre-formatted human size. 1024-based to match `df -h` and the
    /// existing `format_bytes` helper in main.rs.
    pub fn size_display(&self) -> String {
        let bytes = self.size_bytes as f64;
        if self.size_bytes < 1024 {
            format!("{} B", self.size_bytes)
        } else if self.size_bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes / 1024.0)
        } else if self.size_bytes < 1024 * 1024 * 1024 {
            format!("{:.1} MB", bytes / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

/// `files.json` root structure. Versioned so we can migrate without
/// breaking older installs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FilesIndex {
    pub version: u32,
    pub files: Vec<FileRecord>,
}

impl FilesIndex {
    fn new() -> Self {
        Self {
            version: 1,
            files: Vec::new(),
        }
    }

    /// Where the index lives: `~/.local/share/citrate-gui/files.json`
    /// on Linux (same dir as config.json + network data dirs).
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("citrate-gui").join("files.json"))
    }

    /// Load + de-dup. Missing file → empty index. Corrupt file → empty
    /// index with a warning (no panic).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<FilesIndex>(&s) {
                Ok(idx) => idx,
                Err(e) => {
                    tracing::warn!("files.json corrupt, starting fresh: {}", e);
                    FilesIndex::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FilesIndex::new(),
            Err(e) => {
                tracing::warn!("files.json read failed: {}", e);
                FilesIndex::new()
            }
        }
    }

    /// Atomic write — serialize to a tempfile next to the target, then
    /// rename over the target. Prevents a partial-write-then-crash from
    /// leaving the index unreadable.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serialize: {}", e)))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Add/upsert. New entries go to the front so they show up at the
    /// top of the list (newest-first).
    pub fn upsert(&mut self, rec: FileRecord) {
        if let Some(existing) = self.files.iter_mut().find(|f| f.cid == rec.cid) {
            // Keep the existing name if the user has renamed it —
            // don't clobber on re-upload of the same content.
            let preserved_name = existing.name.clone();
            *existing = rec;
            if !preserved_name.is_empty() && preserved_name != existing.name {
                existing.name = preserved_name;
            }
        } else {
            self.files.insert(0, rec);
        }
    }

    /// Remove by CID. Returns the removed record if present.
    pub fn remove(&mut self, cid: &str) -> Option<FileRecord> {
        if let Some(pos) = self.files.iter().position(|f| f.cid == cid) {
            Some(self.files.remove(pos))
        } else {
            None
        }
    }
}

/// Default kubo HTTP API base. Tests point the add/cat helpers at a
/// mock server instead; production callers pass this constant.
pub const DEFAULT_IPFS_API: &str = "http://127.0.0.1:5001";

/// Upload one file to the local IPFS daemon.
///
/// Calls `POST /api/v0/add?pin=true&cid-version=1` on `127.0.0.1:5001`
/// with the file as a multipart/form-data body. Returns the CID +
/// raw byte size parsed from the daemon's response.
pub async fn ipfs_add_file(client: &reqwest::Client, path: &Path) -> Result<(String, u64), String> {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid filename".to_string())?
        .to_string();

    // Read the file upfront. For large files we could stream, but IPFS
    // daemon buffer quirks make a single-shot body more reliable
    // and our storage is mostly small/medium (KB to tens of MB).
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read file: {}", e))?;
    let size = bytes.len() as u64;

    let cid = ipfs_add_bytes(client, DEFAULT_IPFS_API, bytes, filename).await?;
    Ok((cid, size))
}

/// Shared `add` core: POST bytes to `<api_base>/api/v0/add` as
/// multipart/form-data. Both the plaintext and encrypted upload paths
/// funnel through here, and tests can aim it at a mock kubo.
async fn ipfs_add_bytes(
    client: &reqwest::Client,
    api_base: &str,
    bytes: Vec<u8>,
    filename: String,
) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(bytes).file_name(filename);
    let form = reqwest::multipart::Form::new().part("file", part);

    let resp = client
        .post(format!("{}/api/v0/add", api_base))
        // cid-version=1 gives us base32 CIDs which are easier to
        // copy/paste and less ambiguous in URLs.
        .query(&[("pin", "true"), ("cid-version", "1")])
        .timeout(std::time::Duration::from_secs(180))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("ipfs add request: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("ipfs add status {}", resp.status()));
    }

    // IPFS may return multiple NDJSON lines when given a directory;
    // for a single file it's one line.
    let body = resp.text().await.map_err(|e| format!("read body: {}", e))?;
    let last_line = body
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .ok_or_else(|| "empty ipfs response".to_string())?;
    let json: serde_json::Value =
        serde_json::from_str(last_line).map_err(|e| format!("parse ipfs response: {}", e))?;
    let cid = json
        .get("Hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "no Hash in ipfs response".to_string())?
        .to_string();

    Ok(cid)
}

// ===========================================================================
// ENCRYPT-S1 WP-9a — client-side encryption for private uploads
//
// Age-style hybrid envelope (see citrate-federation
// ADR-2026-07-04-ipfs-client-side-encryption):
//
//   file_key   = random 32 bytes                      (one per file)
//   ciphertext = ChaCha20-Poly1305(file_key, nonce, plaintext)
//   eph        = ephemeral X25519 keypair             (one per file)
//   shared     = X25519(eph_secret, owner_pub)
//   wrap_key   = HKDF-SHA256(shared, salt = eph_pub ‖ owner_pub,
//                            info = "citrate-storage-envelope-v1")
//   wrapped    = ChaCha20-Poly1305(wrap_key, 0-nonce, file_key)
//
// The zero wrap-nonce is safe because wrap_key is single-use by
// construction (fresh ephemeral per file — same argument age makes).
// IPFS stores only `ciphertext`; `wrapped_key` + `nonce` live in
// files.json next to the CID. Sharing the CID shares ciphertext.
// ===========================================================================

/// OS-keyring entry (service `citrate-desktop`, the app-wide keyring
/// namespace used by `SystemSecretStore`) holding the hex-encoded
/// 32-byte X25519 static secret for the owner-only envelope.
pub const ENVELOPE_KEYRING_ENTRY: &str = "storage-envelope-x25519-v1";

/// HKDF domain-separation string for the key wrap.
const ENVELOPE_HKDF_INFO: &[u8] = b"citrate-storage-envelope-v1";

/// The device's storage-envelope identity: an X25519 static secret.
///
/// Key-source decision (recorded in the ADR): this is a keyring-held
/// device key, NOT wallet-seed-derived — the wallet seed sits behind
/// the Argon2 unlock prompt and `citrate-wallet-core` deliberately
/// exposes signing only, while the storage tab must work before any
/// wallet exists and without prompting per upload.
pub struct EnvelopeKey {
    secret: x25519_dalek::StaticSecret,
}

impl EnvelopeKey {
    /// Construct from raw secret bytes (tests, future import/export).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: x25519_dalek::StaticSecret::from(bytes),
        }
    }

    /// The recipient public key files get wrapped to.
    pub fn public_bytes(&self) -> [u8; 32] {
        x25519_dalek::PublicKey::from(&self.secret).to_bytes()
    }

    /// Load the envelope secret from the secret store, generating and
    /// persisting a fresh one on first use. Fails (rather than
    /// silently falling back to plaintext) when the store is
    /// unavailable — callers must fail the *upload*, not the privacy.
    pub fn load_or_create(
        store: &dyn citrate_desktop_app::ports::SecretStore,
    ) -> Result<Self, String> {
        if let Some(hex_secret) = store.get_secret(ENVELOPE_KEYRING_ENTRY) {
            let raw = hex::decode(hex_secret.trim())
                .map_err(|e| format!("envelope key in keyring is not hex: {}", e))?;
            let bytes: [u8; 32] = raw
                .try_into()
                .map_err(|_| "envelope key in keyring is not 32 bytes".to_string())?;
            return Ok(Self::from_bytes(bytes));
        }
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        store.set_secret(ENVELOPE_KEYRING_ENTRY, &hex::encode(bytes))?;
        let key = Self::from_bytes(bytes);
        use zeroize::Zeroize;
        bytes.zeroize();
        Ok(key)
    }
}

/// Output of an in-memory envelope encryption.
pub struct EncryptedBlob {
    /// What actually goes to IPFS.
    pub ciphertext: Vec<u8>,
    /// hex(eph_pub(32) ‖ wrapped_file_key(48)) — stored in files.json.
    pub wrapped_key: String,
    /// hex(12-byte content nonce) — stored in files.json.
    pub nonce: String,
}

/// Encrypt `plaintext` so that only the holder of the X25519 secret
/// behind `owner_pub` can read it. Pure function — no I/O — so tests
/// exercise it directly.
pub fn encrypt_for_owner(owner_pub: &[u8; 32], plaintext: &[u8]) -> Result<EncryptedBlob, String> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
    use rand::RngCore;
    use zeroize::Zeroize;

    // Content encryption under a fresh random file key + nonce.
    let mut file_key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut file_key);
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);

    let cipher = ChaCha20Poly1305::new((&file_key).into());
    let ciphertext = cipher
        .encrypt((&nonce).into(), plaintext)
        .map_err(|e| format!("content encrypt: {}", e))?;

    // Wrap the file key to the owner's public key.
    let eph = x25519_dalek::EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let eph_pub = x25519_dalek::PublicKey::from(&eph);
    let owner_pk = x25519_dalek::PublicKey::from(*owner_pub);
    let shared = eph.diffie_hellman(&owner_pk);
    if !shared.was_contributory() {
        file_key.zeroize();
        return Err("owner public key is a low-order point".to_string());
    }
    let mut wrap_key = derive_wrap_key(shared.as_bytes(), &eph_pub.to_bytes(), owner_pub)?;
    let wrap_cipher = ChaCha20Poly1305::new((&wrap_key).into());
    let wrapped = wrap_cipher
        .encrypt((&[0u8; 12]).into(), &file_key[..])
        .map_err(|e| format!("key wrap: {}", e))?;
    file_key.zeroize();
    wrap_key.zeroize();

    let mut wrapped_key_bytes = Vec::with_capacity(32 + wrapped.len());
    wrapped_key_bytes.extend_from_slice(&eph_pub.to_bytes());
    wrapped_key_bytes.extend_from_slice(&wrapped);

    Ok(EncryptedBlob {
        ciphertext,
        wrapped_key: hex::encode(wrapped_key_bytes),
        nonce: hex::encode(nonce),
    })
}

/// Reverse of [`encrypt_for_owner`] — unwrap the file key with the
/// owner's secret, then decrypt the content. Any tamper / wrong key
/// fails the AEAD tag and returns `Err`.
///
/// (dead_code allowance: the read-back UI lands with the NATIVE-R1-S3
/// storage rebuild; until then this is exercised by the WP-9 tests.)
#[allow(dead_code)]
pub fn decrypt_with_key(
    key: &EnvelopeKey,
    ciphertext: &[u8],
    wrapped_key_hex: &str,
    nonce_hex: &str,
) -> Result<Vec<u8>, String> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
    use zeroize::Zeroize;

    let wrapped_all =
        hex::decode(wrapped_key_hex).map_err(|e| format!("wrapped_key not hex: {}", e))?;
    if wrapped_all.len() != 32 + 32 + 16 {
        return Err(format!("wrapped_key wrong length: {}", wrapped_all.len()));
    }
    let eph_pub_bytes: [u8; 32] = wrapped_all[..32].try_into().expect("checked length");
    let wrapped = &wrapped_all[32..];

    let nonce_raw = hex::decode(nonce_hex).map_err(|e| format!("nonce not hex: {}", e))?;
    let nonce: [u8; 12] = nonce_raw
        .try_into()
        .map_err(|_| "nonce is not 12 bytes".to_string())?;

    let eph_pub = x25519_dalek::PublicKey::from(eph_pub_bytes);
    let shared = key.secret.diffie_hellman(&eph_pub);
    if !shared.was_contributory() {
        return Err("ephemeral public key is a low-order point".to_string());
    }
    let owner_pub = key.public_bytes();
    let mut wrap_key = derive_wrap_key(shared.as_bytes(), &eph_pub_bytes, &owner_pub)?;
    let wrap_cipher = ChaCha20Poly1305::new((&wrap_key).into());
    let file_key_vec = wrap_cipher
        .decrypt((&[0u8; 12]).into(), wrapped)
        .map_err(|_| "key unwrap failed (wrong key or corrupt record)".to_string())?;
    wrap_key.zeroize();
    let mut file_key: [u8; 32] = file_key_vec
        .try_into()
        .map_err(|_| "unwrapped file key is not 32 bytes".to_string())?;

    let cipher = ChaCha20Poly1305::new((&file_key).into());
    let plaintext = cipher
        .decrypt((&nonce).into(), ciphertext)
        .map_err(|_| "content decrypt failed (wrong key or corrupt blob)".to_string());
    file_key.zeroize();
    plaintext
}

/// HKDF-SHA256 step shared by wrap + unwrap.
fn derive_wrap_key(
    shared: &[u8],
    eph_pub: &[u8; 32],
    owner_pub: &[u8; 32],
) -> Result<[u8; 32], String> {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(eph_pub);
    salt[32..].copy_from_slice(owner_pub);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 32];
    hk.expand(ENVELOPE_HKDF_INFO, &mut okm)
        .map_err(|e| format!("hkdf expand: {}", e))?;
    Ok(okm)
}

/// Result of an encrypted upload — everything the caller needs to
/// build the `FileRecord`.
pub struct EncryptedAdd {
    /// CID of the CIPHERTEXT.
    pub cid: String,
    /// Plaintext size (what the user recognizes in the file list).
    pub size_bytes: u64,
    pub wrapped_key: String,
    pub nonce: String,
}

/// Encrypt-before-add: read the file, seal it for `owner_pub`, and
/// upload only the ciphertext. The multipart filename is a constant
/// (`private.bin`) so the original name never reaches the daemon —
/// the friendly name lives exclusively in files.json.
pub async fn ipfs_add_file_encrypted(
    client: &reqwest::Client,
    api_base: &str,
    path: &Path,
    owner_pub: &[u8; 32],
) -> Result<EncryptedAdd, String> {
    let plaintext = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read file: {}", e))?;
    let size_bytes = plaintext.len() as u64;

    let blob = encrypt_for_owner(owner_pub, &plaintext)?;
    drop(plaintext);

    let cid = ipfs_add_bytes(client, api_base, blob.ciphertext, "private.bin".to_string()).await?;

    Ok(EncryptedAdd {
        cid,
        size_bytes,
        wrapped_key: blob.wrapped_key,
        nonce: blob.nonce,
    })
}

/// Fetch raw bytes for a CID from the daemon (`/api/v0/cat`).
///
/// (dead_code allowance: see `decrypt_with_key`.)
#[allow(dead_code)]
pub async fn ipfs_cat(
    client: &reqwest::Client,
    api_base: &str,
    cid: &str,
) -> Result<Vec<u8>, String> {
    let resp = client
        .post(format!("{}/api/v0/cat", api_base))
        .query(&[("arg", cid)])
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("ipfs cat request: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("ipfs cat status {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("read cat body: {}", e))
}

/// Decrypt-on-fetch helper: cat the ciphertext behind `rec.cid` and
/// open the envelope. This is the read-back path the NATIVE-R1-S3
/// storage rebuild (download/open buttons) will call.
///
/// (dead_code allowance: see `decrypt_with_key`.)
#[allow(dead_code)]
pub async fn ipfs_fetch_decrypt(
    client: &reqwest::Client,
    api_base: &str,
    key: &EnvelopeKey,
    rec: &FileRecord,
) -> Result<Vec<u8>, String> {
    if !rec.encrypted {
        return ipfs_cat(client, api_base, &rec.cid).await;
    }
    let ciphertext = ipfs_cat(client, api_base, &rec.cid).await?;
    decrypt_with_key(key, &ciphertext, &rec.wrapped_key, &rec.nonce)
}

/// Remove a pin. Does NOT delete from the DHT (can't on a public
/// network) but drops our local pin so the GC reclaims bytes.
pub async fn ipfs_unpin(client: &reqwest::Client, cid: &str) -> Result<(), String> {
    let resp = client
        .post("http://127.0.0.1:5001/api/v0/pin/rm")
        .query(&[("arg", cid)])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("ipfs unpin request: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("ipfs unpin status {}", resp.status()));
    }
    Ok(())
}

/// Public share link. `ipfs://<cid>` is the canonical form;
/// most browsers / apps handle it via an extension or gateway.
pub fn share_link(cid: &str) -> String {
    format!("ipfs://{}", cid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_icons_match_major_type() {
        let cases: &[(&str, &str)] = &[
            ("image/png", "🖼"),
            ("video/mp4", "🎬"),
            ("audio/mpeg", "🎵"),
            ("text/plain", "📄"),
            ("application/pdf", "📕"),
            ("application/zip", "🗜"),
            ("application/json", "📑"),
            ("application/octet-stream", "📦"),
            ("weird/unknown", "📁"),
        ];
        for (mime, expected) in cases {
            let rec = FileRecord {
                cid: "cid".into(),
                name: "f".into(),
                size_bytes: 1,
                uploaded_at: 0,
                mime: (*mime).into(),
                ..Default::default()
            };
            assert_eq!(rec.mime_icon(), *expected, "mime: {}", mime);
        }
    }

    #[test]
    fn size_display_crosses_unit_boundaries() {
        let tests: &[(u64, &str)] = &[
            (0, "0 B"),
            (512, "512 B"),
            (1024, "1.0 KB"),
            (1_048_576, "1.0 MB"),
            (1_073_741_824, "1.00 GB"),
        ];
        for (bytes, expected) in tests {
            let rec = FileRecord {
                cid: "c".into(),
                name: "f".into(),
                size_bytes: *bytes,
                uploaded_at: 0,
                mime: "text/plain".into(),
                ..Default::default()
            };
            assert_eq!(rec.size_display(), *expected, "bytes: {}", bytes);
        }
    }

    #[test]
    fn uploaded_display_relative_buckets() {
        let rec = |uploaded_at| FileRecord {
            cid: "c".into(),
            name: "f".into(),
            size_bytes: 1,
            uploaded_at,
            mime: "text/plain".into(),
            ..Default::default()
        };
        let now = 1_000_000u64;
        assert_eq!(rec(now - 5).uploaded_display(now), "just now");
        assert_eq!(rec(now - 125).uploaded_display(now), "2m ago");
        assert_eq!(rec(now - 7200).uploaded_display(now), "2h ago");
        // > 48h → days ago
        let three_days = 3 * 86_400;
        assert_eq!(rec(now - three_days).uploaded_display(now), "3 days ago");
        // Defensive: future timestamp → "—"
        assert_eq!(rec(now + 100).uploaded_display(now), "—");
    }

    #[test]
    fn index_roundtrip_preserves_entries() {
        let tmp =
            std::env::temp_dir().join(format!("citrate-storage-test-{}.json", std::process::id()));
        let mut idx = FilesIndex::new();
        idx.upsert(FileRecord {
            cid: "bafy1".into(),
            name: "resume.pdf".into(),
            size_bytes: 12345,
            uploaded_at: 1_700_000_000,
            mime: "application/pdf".into(),
            ..Default::default()
        });
        idx.upsert(FileRecord {
            cid: "bafy2".into(),
            name: "photo.png".into(),
            size_bytes: 99_999,
            uploaded_at: 1_700_100_000,
            mime: "image/png".into(),
            ..Default::default()
        });
        idx.save(&tmp).expect("save ok");
        let loaded = FilesIndex::load(&tmp);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.files.len(), 2);
        // Newest-first insertion order preserved
        assert_eq!(loaded.files[0].cid, "bafy2");
        assert_eq!(loaded.files[1].cid, "bafy1");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn index_upsert_preserves_user_rename() {
        let mut idx = FilesIndex::new();
        idx.upsert(FileRecord {
            cid: "bafy".into(),
            name: "original.pdf".into(),
            size_bytes: 100,
            uploaded_at: 0,
            mime: "application/pdf".into(),
            ..Default::default()
        });
        // Simulate user rename
        idx.files[0].name = "My Résumé.pdf".into();
        // Re-upsert same CID with a different "name" (e.g., re-upload
        // from a different source path).
        idx.upsert(FileRecord {
            cid: "bafy".into(),
            name: "resume_v2.pdf".into(),
            size_bytes: 100,
            uploaded_at: 0,
            mime: "application/pdf".into(),
            ..Default::default()
        });
        // The renamed name should survive.
        assert_eq!(idx.files[0].name, "My Résumé.pdf");
    }

    #[test]
    fn share_link_uses_ipfs_scheme() {
        assert_eq!(
            share_link("bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"),
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        );
    }

    // =====================================================================
    // ENCRYPT-S1 WP-9a — envelope + encrypted upload tests
    // =====================================================================

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// In-memory `SecretStore` so tests never touch the OS keyring —
    /// same test-backend pattern as `citrate_desktop_app::ports` tests.
    #[derive(Default)]
    struct InMemorySecrets {
        data: Mutex<HashMap<String, String>>,
    }

    impl citrate_desktop_app::ports::SecretStore for InMemorySecrets {
        fn get_secret(&self, key: &str) -> Option<String> {
            self.data.lock().expect("mutex").get(key).cloned()
        }
        fn set_secret(&self, key: &str, value: &str) -> Result<(), String> {
            self.data
                .lock()
                .expect("mutex")
                .insert(key.into(), value.into());
            Ok(())
        }
        fn delete_secret(&self, key: &str) -> Result<(), String> {
            self.data.lock().expect("mutex").remove(key);
            Ok(())
        }
        fn has_secret(&self, key: &str) -> bool {
            self.data.lock().expect("mutex").contains_key(key)
        }
    }

    /// Minimal mock kubo HTTP API (`/api/v0/add` + `/api/v0/cat`) on an
    /// ephemeral port. Captures every raw `add` request body so tests
    /// can probe exactly what would go over the wire to the daemon,
    /// and serves stored blobs back through `cat` for roundtrips.
    struct MockKubo {
        base_url: String,
        /// Raw multipart bodies of every /api/v0/add request.
        add_bodies: Arc<Mutex<Vec<Vec<u8>>>>,
        /// cid → stored file bytes (extracted from the multipart).
        blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack.windows(needle.len()).rposition(|w| w == needle)
    }

    /// Extract the file part's content from a single-part multipart
    /// body: content sits between the first blank line and the final
    /// `\r\n--` boundary marker. Crude but sufficient for a test peer.
    fn multipart_file_content(body: &[u8]) -> Vec<u8> {
        let start = find_subslice(body, b"\r\n\r\n").map(|i| i + 4).unwrap_or(0);
        let end = rfind_subslice(body, b"\r\n--").unwrap_or(body.len());
        body[start..end.max(start)].to_vec()
    }

    async fn spawn_mock_kubo() -> MockKubo {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock kubo");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let add_bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let blobs: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

        let bodies_srv = add_bodies.clone();
        let blobs_srv = blobs.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let bodies = bodies_srv.clone();
                let blobs = blobs_srv.clone();
                tokio::spawn(async move {
                    // Read headers.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end = loop {
                        match sock.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            Err(_) => return,
                        }
                        if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let request_line = headers.lines().next().unwrap_or("").to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    // Read the remainder of the body.
                    while buf.len() < header_end + content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            Err(_) => return,
                        }
                    }
                    let body = buf[header_end..].to_vec();

                    let response = if request_line.contains("/api/v0/add") {
                        let content = multipart_file_content(&body);
                        let cid = {
                            let mut blobs = blobs.lock().expect("mutex");
                            let cid = format!("bafymock{}", blobs.len() + 1);
                            blobs.insert(cid.clone(), content.clone());
                            cid
                        };
                        bodies.lock().expect("mutex").push(body);
                        let json = format!(
                            "{{\"Name\":\"f\",\"Hash\":\"{}\",\"Size\":\"{}\"}}\n",
                            cid,
                            content.len()
                        );
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            json.len(),
                            json
                        )
                        .into_bytes()
                    } else if request_line.contains("/api/v0/cat") {
                        // arg=<cid> query param.
                        let cid = request_line
                            .split("arg=")
                            .nth(1)
                            .and_then(|s| s.split(&['&', ' '][..]).next())
                            .unwrap_or("")
                            .to_string();
                        match blobs.lock().expect("mutex").get(&cid) {
                            Some(bytes) => {
                                let mut resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    bytes.len()
                                )
                                .into_bytes();
                                resp.extend_from_slice(bytes);
                                resp
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        }
                    } else {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    };
                    let _ = sock.write_all(&response).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        MockKubo {
            base_url,
            add_bodies,
            blobs,
        }
    }

    fn write_temp_file(name_hint: &str, contents: &[u8]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("citrate-wp9-{}-{}", std::process::id(), name_hint));
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    #[test]
    fn envelope_roundtrip_in_memory() {
        let key = EnvelopeKey::from_bytes([7u8; 32]);
        let plaintext = b"the plans for the salt mine";
        let blob = encrypt_for_owner(&key.public_bytes(), plaintext).expect("encrypt");
        assert_ne!(blob.ciphertext, plaintext.to_vec());
        assert_eq!(
            blob.wrapped_key.len(),
            (32 + 32 + 16) * 2,
            "hex(eph_pub‖wrapped)"
        );
        assert_eq!(blob.nonce.len(), 24, "hex(12-byte nonce)");
        let opened = decrypt_with_key(&key, &blob.ciphertext, &blob.wrapped_key, &blob.nonce)
            .expect("decrypt");
        assert_eq!(opened, plaintext.to_vec());
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let owner = EnvelopeKey::from_bytes([7u8; 32]);
        let intruder = EnvelopeKey::from_bytes([8u8; 32]);
        let blob = encrypt_for_owner(&owner.public_bytes(), b"private").expect("encrypt");
        let err = decrypt_with_key(&intruder, &blob.ciphertext, &blob.wrapped_key, &blob.nonce);
        assert!(
            err.is_err(),
            "a different X25519 secret must not open the envelope"
        );
        // Tampered ciphertext fails too (AEAD tag).
        let mut tampered = blob.ciphertext.clone();
        tampered[0] ^= 0x01;
        let err = decrypt_with_key(&owner, &tampered, &blob.wrapped_key, &blob.nonce);
        assert!(err.is_err(), "tampered ciphertext must fail the tag check");
    }

    #[test]
    fn envelope_key_load_or_create_persists_and_reloads() {
        let store = InMemorySecrets::default();
        let first = EnvelopeKey::load_or_create(&store).expect("create");
        assert!(
            citrate_desktop_app::ports::SecretStore::has_secret(&store, ENVELOPE_KEYRING_ENTRY),
            "secret persisted to the store"
        );
        let second = EnvelopeKey::load_or_create(&store).expect("reload");
        assert_eq!(
            first.public_bytes(),
            second.public_bytes(),
            "reload must yield the same identity"
        );
    }

    #[tokio::test]
    async fn encrypted_roundtrip_add_fetch_decrypt() {
        let kubo = spawn_mock_kubo().await;
        let client = reqwest::Client::new();
        let key = EnvelopeKey::from_bytes([42u8; 32]);
        let plaintext = b"WP-9 roundtrip: encrypt, add, cat, decrypt".to_vec();
        let path = write_temp_file("roundtrip.txt", &plaintext);

        let added = ipfs_add_file_encrypted(&client, &kubo.base_url, &path, &key.public_bytes())
            .await
            .expect("encrypted add");
        assert_eq!(
            added.size_bytes,
            plaintext.len() as u64,
            "records plaintext size"
        );

        let rec = FileRecord {
            cid: added.cid.clone(),
            name: "roundtrip.txt".into(),
            size_bytes: added.size_bytes,
            uploaded_at: 0,
            mime: "text/plain".into(),
            encrypted: true,
            wrapped_key: added.wrapped_key.clone(),
            nonce: added.nonce.clone(),
        };
        let fetched = ipfs_fetch_decrypt(&client, &kubo.base_url, &key, &rec)
            .await
            .expect("fetch + decrypt");
        assert_eq!(fetched, plaintext);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ciphertext_on_wire_not_plaintext() {
        let kubo = spawn_mock_kubo().await;
        let client = reqwest::Client::new();
        let key = EnvelopeKey::from_bytes([9u8; 32]);
        // Distinctive marker so a substring probe is meaningful.
        let plaintext = b"TOPSECRET-MARKER-0xC17RA7E do not leak TOPSECRET-MARKER".to_vec();
        let path = write_temp_file("wire-probe.bin", &plaintext);

        ipfs_add_file_encrypted(&client, &kubo.base_url, &path, &key.public_bytes())
            .await
            .expect("encrypted add");

        {
            let bodies = kubo.add_bodies.lock().expect("mutex");
            assert_eq!(bodies.len(), 1);
            assert!(
                find_subslice(&bodies[0], b"TOPSECRET-MARKER").is_none(),
                "plaintext must NOT appear in the bytes handed to the daemon"
            );
            assert!(
                find_subslice(&bodies[0], b"wire-probe").is_none(),
                "original filename must not reach the daemon either"
            );
        }
        // What the daemon stored is ciphertext, not the file.
        {
            let blobs = kubo.blobs.lock().expect("mutex");
            let stored = blobs.values().next().expect("one blob stored");
            assert_ne!(stored, &plaintext);
            assert_eq!(stored.len(), plaintext.len() + 16, "AEAD tag overhead only");
        }

        // Positive control: the same probe on a PLAINTEXT add does see
        // the marker — proving the probe itself works.
        ipfs_add_bytes(
            &client,
            &kubo.base_url,
            plaintext.clone(),
            "control.bin".into(),
        )
        .await
        .expect("plaintext add");
        let bodies = kubo.add_bodies.lock().expect("mutex");
        assert!(
            find_subslice(&bodies[1], b"TOPSECRET-MARKER").is_some(),
            "control: plaintext add carries the marker on the wire"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn files_json_backward_compat_pre_encryption_records() {
        // A pre-WP-9 files.json — no encrypted/wrapped_key/nonce fields.
        let legacy = r#"{
            "version": 1,
            "files": [{
                "cid": "bafyold",
                "name": "old.pdf",
                "size_bytes": 123,
                "uploaded_at": 1700000000,
                "mime": "application/pdf"
            }]
        }"#;
        let idx: FilesIndex = serde_json::from_str(legacy).expect("legacy index parses");
        assert_eq!(idx.files.len(), 1);
        let f = &idx.files[0];
        assert!(!f.encrypted, "legacy records default to plaintext");
        assert!(f.wrapped_key.is_empty());
        assert!(f.nonce.is_empty());

        // And a legacy-shaped record serializes WITHOUT the empty wrap
        // fields (skip_serializing_if), so old builds see familiar JSON.
        let json = serde_json::to_string(f).expect("serialize");
        assert!(!json.contains("wrapped_key"));
        assert!(!json.contains("nonce"));

        // Mixed index roundtrips through save/load with fields intact.
        let tmp =
            std::env::temp_dir().join(format!("citrate-wp9-compat-{}.json", std::process::id()));
        let mut idx = idx;
        idx.upsert(FileRecord {
            cid: "bafyenc".into(),
            name: "sealed.bin".into(),
            size_bytes: 9,
            uploaded_at: 1_700_000_001,
            mime: "application/octet-stream".into(),
            encrypted: true,
            wrapped_key: "aa".repeat(80),
            nonce: "bb".repeat(12),
        });
        idx.save(&tmp).expect("save");
        let loaded = FilesIndex::load(&tmp);
        assert_eq!(loaded.files.len(), 2);
        assert!(loaded.files[0].encrypted);
        assert_eq!(loaded.files[0].wrapped_key, "aa".repeat(80));
        assert_eq!(loaded.files[0].nonce, "bb".repeat(12));
        assert!(!loaded.files[1].encrypted);
        let _ = std::fs::remove_file(&tmp);
    }
}
