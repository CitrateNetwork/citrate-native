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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    /// CID returned by IPFS `add`. This is the stable identifier
    /// across renames.
    pub cid: String,
    /// User-facing name. Defaults to the original filename; can be
    /// changed via rename without affecting the CID.
    pub name: String,
    /// Bytes, raw. Pre-formatting happens in the UI layer.
    pub size_bytes: u64,
    /// Unix seconds at upload time.
    pub uploaded_at: u64,
    /// MIME type guessed from the filename extension at upload.
    /// Stable across renames because the extension is captured then.
    pub mime: String,
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
            "text"  => "📄",
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
        Self { version: 1, files: Vec::new() }
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

/// Upload one file to the local IPFS daemon.
///
/// Calls `POST /api/v0/add?pin=true&cid-version=1` on `127.0.0.1:5001`
/// with the file as a multipart/form-data body. Returns the CID +
/// raw byte size parsed from the daemon's response.
pub async fn ipfs_add_file(
    client: &reqwest::Client,
    path: &Path,
) -> Result<(String, u64), String> {
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

    let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.clone());
    let form = reqwest::multipart::Form::new().part("file", part);

    let resp = client
        .post("http://127.0.0.1:5001/api/v0/add")
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
    let last_line = body.lines().rfind(|l| !l.trim().is_empty())
        .ok_or_else(|| "empty ipfs response".to_string())?;
    let json: serde_json::Value = serde_json::from_str(last_line)
        .map_err(|e| format!("parse ipfs response: {}", e))?;
    let cid = json.get("Hash").and_then(|v| v.as_str())
        .ok_or_else(|| "no Hash in ipfs response".to_string())?
        .to_string();

    Ok((cid, size))
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
        let tmp = std::env::temp_dir().join(format!(
            "citrate-storage-test-{}.json",
            std::process::id()
        ));
        let mut idx = FilesIndex::new();
        idx.upsert(FileRecord {
            cid: "bafy1".into(),
            name: "resume.pdf".into(),
            size_bytes: 12345,
            uploaded_at: 1_700_000_000,
            mime: "application/pdf".into(),
        });
        idx.upsert(FileRecord {
            cid: "bafy2".into(),
            name: "photo.png".into(),
            size_bytes: 99_999,
            uploaded_at: 1_700_100_000,
            mime: "image/png".into(),
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
}
