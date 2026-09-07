//! NATIVE-R1-S2 WP-A1: node-start crash telemetry.
//!
//! Background: one survey session died SILENTLY right after
//! "Node started via settings" — no panic output, no coredump, no OOM
//! record. This module makes the *next* occurrence forensically useful:
//!
//! 1. **Panic hook** (`install_panic_hook`) — installed first thing in
//!    `main()`, before the tracing subscriber. Any unwinding panic on ANY
//!    thread writes a JSON crash record (message, location, backtrace,
//!    timestamp, app version, last-known app state) under
//!    `~/.local/share/citrate-gui/crash/`, then chains to the previous
//!    hook so the panic still prints to stderr.
//!
//! 2. **Session marker** — aborts, SIGKILL, and segfaults never run the
//!    panic hook, so we also write `crash/session.marker` at boot and
//!    remove it on clean exit (`mark_clean_exit`). The marker is
//!    re-written on every `set_last_state` call, so it always carries the
//!    last known app state. A marker found at the NEXT launch
//!    (`startup_scan`) proves the previous session died uncleanly; the
//!    scan converts it into an `unclean-exit` crash record and logs both
//!    paths prominently (tracing WARN).
//!
//! 3. **Rotating file log** (`file_log_writer`) — a `MakeWriter` for the
//!    tracing subscriber that appends to
//!    `~/.local/share/citrate-gui/logs/citrate-gui.log` and rotates to
//!    `citrate-gui.log.1` at 5 MB, so the silent-death forensics have
//!    logs that survive the terminal. Dependency-free (no
//!    tracing-appender).
//!
//! 4. **Disk redaction** (ENCRYPT-S1 WP-4, inventory A7/A8) —
//!    `redact_for_disk` scrubs everything this module writes to disk
//!    (crash records, session.marker, the rotating file log): 0x-hex
//!    addresses and 64-hex hashes are truncated to first-6…last-4, and
//!    balance/amount-adjacent numbers become `<redacted>`. Backtraces
//!    keep their symbols (needed for forensics — frame addresses are
//!    short code pointers, not user data). stderr stays UNREDACTED —
//!    the local terminal is inside the trust boundary; disk is not.
//!
//! Data source (Rule 11): the crash-record files themselves + the
//! injected-panic child-process test in this module's test suite.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// App version baked into every crash record.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment override for the app data dir — used by the injected-panic
/// child-process test to redirect crash records into a temp dir. In
/// production this is unset and the default path is used.
pub const DATA_DIR_ENV: &str = "CITRATE_GUI_DATA_DIR";

/// Rotate the tracing file log when it exceeds this many bytes.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Last-known app state, updated at key transitions (node start/stop,
/// event-loop entry, …). Included in every crash record AND persisted
/// into the session marker so it survives even a SIGKILL.
static LAST_STATE: Mutex<Option<String>> = Mutex::new(None);

/// Path of the live session marker once `init_session_marker` has run.
static MARKER_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Monotonic suffix so two crash records in the same millisecond
/// (e.g. simultaneous thread panics) never collide on filename.
static RECORD_SEQ: AtomicU32 = AtomicU32::new(0);

// =========================================================================
// Paths
// =========================================================================

/// App data dir: `$CITRATE_GUI_DATA_DIR` override, else
/// `~/.local/share/citrate-gui` (per-platform via `dirs`), else temp.
pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("citrate-gui")
}

/// Where crash records + the session marker live.
pub fn crash_dir() -> PathBuf {
    app_data_dir().join("crash")
}

/// Where the rotating tracing file log lives.
pub fn logs_dir() -> PathBuf {
    app_data_dir().join("logs")
}

fn marker_path_in(crash_dir: &Path) -> PathBuf {
    crash_dir.join("session.marker")
}

// =========================================================================
// Disk redaction (ENCRYPT-S1 WP-4, inventory A7/A8)
// =========================================================================

/// `balance`/`amount`-adjacent values: `balance=1234`, `"amount": 5.5`,
/// `total_amount = 0xdead…`. The key (group 1) is kept; the value is
/// replaced with `<redacted>`. Case-insensitive; tolerates `:`/`=`/
/// quotes/whitespace between key and value; catches hex values too so
/// the truncating hex pass (which runs second) never sees them.
fn balance_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)([a-z0-9_]*(?:balance|amount)[a-z0-9_]*\s*"?\s*[:=]?\s*"?\s*)(-?(?:0x[0-9a-fA-F]+|[0-9][0-9_.,]*))"#,
        )
        .expect("balance redaction regex is valid")
    })
}

/// Long hex material: 0x-prefixed runs of ≥40 hex chars (EVM addresses
/// and anything bigger — hashes, signatures) and bare runs of ≥64 hex
/// chars (tx/state hashes without the 0x). Truncated, not dropped —
/// first 6 + last 4 chars keep records correlatable across a crash
/// report without exposing the full identifier.
fn hex_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\b0x[0-9a-fA-F]{40,}\b|\b[0-9a-fA-F]{64,}\b")
            .expect("hex redaction regex is valid")
    })
}

/// NAT-B-005: secret-bearing key/value pairs. Unlike the balance pass
/// (which keeps a couple of numeric fields readable for forensics), any
/// value keyed by a secret-bearing identifier is DROPPED wholesale — a
/// mnemonic, password, passphrase, PIN, token, API key, secret, or
/// private key must never reach disk even truncated. Case-insensitive;
/// tolerates `:`/`=`, quotes, and whitespace between key and value; the
/// value runs to the next quote, comma, or whitespace.
fn secret_kv_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)([a-z0-9_]*(?:mnemonic|seed(?:_?phrase)?|passphrase|password|pass|api[_-]?key|secret|token|private[_-]?key|priv[_-]?key|pin)[a-z0-9_]*\s*"?\s*[:=]\s*"?\s*)([^\s",}]+)"#,
        )
        .expect("secret key/value redaction regex is valid")
    })
}

/// NAT-B-005: Argon2 PHC strings (`$argon2id$v=19$m=...$salt$hash`). The
/// keystore's password-hash encoding must never be written to disk — it
/// is offline-crackable material.
fn phc_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\$argon2(?:id|i|d)\$[A-Za-z0-9$=,+/._-]+")
            .expect("PHC redaction regex is valid")
    })
}

/// NAT-B-005: a BIP-39 mnemonic printed bare (no key label) — a run of
/// 12+ space-separated lowercase words of 3–8 letters, the shape of every
/// BIP-39 wordlist entry. Redacts the whole run. This is deliberately
/// broad: on the crash/log disk boundary, over-redacting a rare 12-word
/// lowercase prose run is strictly preferable to leaking recovery-phrase
/// words. Backtraces (symbols carry `::`, digits, capitals) never match.
fn mnemonic_run_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\b[a-z]{3,8}(?: [a-z]{3,8}){11,}\b")
            .expect("mnemonic-run redaction regex is valid")
    })
}

/// Redaction pass applied to everything written to DISK by this module
/// (crash-record message/last_state, session.marker last_state, and
/// every file-log line). stderr output is deliberately not routed
/// through this — the local terminal is the operator's own screen.
///
/// Applied to field values BEFORE serialization, so redacted crash
/// records and markers remain valid JSON.
pub fn redact_for_disk(text: &str) -> String {
    // Pass 0 (NAT-B-005): drop key/value secrets, PHC strings, and bare
    // mnemonic runs BEFORE the hex/balance passes so no secret survives
    // as a "truncated" identifier.
    let pass0 = secret_kv_re().replace_all(text, "${1}<redacted>");
    let pass0 = phc_re().replace_all(&pass0, "<redacted>");
    let pass0 = mnemonic_run_re().replace_all(&pass0, "<redacted>");
    // Pass 1: balance/amount values (including hex values) → <redacted>.
    let pass1 = balance_re().replace_all(&pass0, "${1}<redacted>");
    // Pass 2: remaining long hex identifiers → first-6…last-4.
    hex_re()
        .replace_all(&pass1, |caps: &regex::Captures<'_>| {
            let m = caps.get(0).expect("whole match").as_str();
            // Matches are pure ASCII, so byte slicing is char-safe.
            format!("{}…{}", &m[..6], &m[m.len() - 4..])
        })
        .into_owned()
}

// =========================================================================
// Filesystem permissions (NAT-B-005)
// =========================================================================

/// Restrict a just-written file to owner-only (0600) on Unix. Crash
/// records, the session marker, and the rotating log can carry
/// user-identifying breadcrumbs; a world-readable (0644) copy is readable
/// by every local user and by any process running as the user (backup
/// agents, sync clients). No-op on non-Unix. Best-effort — a failure
/// here must never break telemetry.
pub fn restrict_file_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Restrict a just-created directory to owner-only (0700) on Unix so its
/// contents are not enumerable by other local users. No-op elsewhere.
fn restrict_dir_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

// =========================================================================
// Crash records
// =========================================================================

/// One crash record = one JSON file under `crash/`.
///
/// `kind` is `"panic"` (hook fired: message/location/backtrace are real)
/// or `"unclean-exit"` (previous process died without running ANY hook —
/// abort/SIGKILL/segfault — reconstructed from the stale session marker,
/// so backtrace is unavailable by definition).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CrashRecord {
    pub timestamp: String,
    pub app_version: String,
    pub kind: String,
    pub thread: String,
    pub message: String,
    pub location: String,
    pub backtrace: String,
    pub last_state: String,
}

/// Write a crash record as pretty JSON into `dir`, returning the path.
/// Must never panic — it runs inside the panic hook.
///
/// ENCRYPT-S1 WP-4: `message` and `last_state` are passed through
/// [`redact_for_disk`] before serialization — panic messages and
/// breadcrumbs can embed addresses/balances/state. `backtrace` is
/// written verbatim: symbols are the forensic payload and frame
/// addresses are code pointers, not user data.
pub fn write_crash_record(dir: &Path, record: &CrashRecord) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    restrict_dir_perms(dir);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = RECORD_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("crash-{millis}-{seq}.json"));
    let sanitized = CrashRecord {
        timestamp: record.timestamp.clone(),
        app_version: record.app_version.clone(),
        kind: record.kind.clone(),
        thread: record.thread.clone(),
        message: redact_for_disk(&record.message),
        location: record.location.clone(),
        backtrace: record.backtrace.clone(),
        last_state: redact_for_disk(&record.last_state),
    };
    let json = serde_json::to_vec_pretty(&sanitized)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)?;
    restrict_file_perms(&path);
    Ok(path)
}

fn last_state() -> String {
    LAST_STATE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "<no state recorded>".to_string())
}

/// Record the last-known app state (e.g. `"node-start (settings)"`).
/// Also re-writes the session marker so the state survives SIGKILL.
pub fn set_last_state(state: &str) {
    if let Ok(mut guard) = LAST_STATE.lock() {
        *guard = Some(state.to_string());
    }
    // Persist into the live marker (best-effort) so an abort/kill after
    // this point is attributable to `state` on the next launch.
    if let Ok(guard) = MARKER_PATH.lock() {
        if let Some(path) = guard.as_ref() {
            let _ = write_marker_file(path, state);
        }
    }
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install the crash-record panic hook. Call FIRST THING in `main()`,
/// before the tracing subscriber (the hook only needs stderr + the
/// filesystem). Covers panics on any thread — `std::panic::set_hook`
/// is process-global and runs before unwinding starts, so it fires even
/// for panics that would abort (panic-in-panic, `panic = "abort"`).
pub fn install_panic_hook() {
    // Make the chained default hook print a backtrace too. The record's
    // own backtrace uses `force_capture` and does not depend on this.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let record = CrashRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_version: APP_VERSION.to_string(),
            kind: "panic".to_string(),
            thread: std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_string(),
            message: panic_message(info),
            location: info
                .location()
                .map(|l| l.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
            last_state: last_state(),
        };
        match write_crash_record(&crash_dir(), &record) {
            Ok(path) => eprintln!("[crash-telemetry] crash record written: {}", path.display()),
            Err(e) => eprintln!("[crash-telemetry] FAILED to write crash record: {e}"),
        }
        // Still print the panic (message + backtrace) to stderr.
        prev(info);
    }));
}

// =========================================================================
// Session marker (abort / SIGKILL / segfault detection)
// =========================================================================

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SessionMarker {
    pid: u32,
    app_version: String,
    started_at: String,
    last_state: String,
    updated_at: String,
}

fn write_marker_file(path: &Path, state: &str) -> io::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let marker = SessionMarker {
        pid: std::process::id(),
        app_version: APP_VERSION.to_string(),
        // Preserve original start time if the marker already exists.
        started_at: std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionMarker>(&b).ok())
            .map(|m| m.started_at)
            .unwrap_or_else(|| now.clone()),
        // ENCRYPT-S1 WP-4: the marker persists the breadcrumb across
        // SIGKILL — scrub it at the disk boundary.
        last_state: redact_for_disk(state),
        updated_at: now,
    };
    let json = serde_json::to_vec_pretty(&marker)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(path, json)?;
    restrict_file_perms(path);
    Ok(())
}

/// Write the "session started" marker. An unclean death (SIGKILL,
/// segfault, abort — anything that never runs the panic hook) leaves it
/// behind; `startup_scan` on the NEXT launch detects it. Call AFTER
/// `startup_scan` (else the fresh marker looks stale).
pub fn init_session_marker() {
    init_session_marker_in(&crash_dir());
}

fn init_session_marker_in(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("crash-telemetry: cannot create {}: {e}", dir.display());
        return;
    }
    restrict_dir_perms(dir);
    let path = marker_path_in(dir);
    if let Err(e) = write_marker_file(&path, "session-start") {
        tracing::warn!("crash-telemetry: cannot write session marker: {e}");
        return;
    }
    if let Ok(mut guard) = MARKER_PATH.lock() {
        *guard = Some(path);
    }
}

/// Remove the session marker — call on the clean-exit path only.
pub fn mark_clean_exit() {
    if let Ok(guard) = MARKER_PATH.lock() {
        if let Some(path) = guard.as_ref() {
            match std::fs::remove_file(path) {
                Ok(()) => tracing::info!("crash-telemetry: clean exit, session marker removed"),
                Err(e) => {
                    tracing::warn!("crash-telemetry: failed to remove session marker: {e}")
                }
            }
        }
    }
}

/// Findings from `startup_scan`, mostly for tests; production just logs.
#[derive(Debug, Default)]
pub struct StartupScanReport {
    /// Set iff a stale session marker was found (previous unclean death);
    /// contains the path of the `unclean-exit` crash record written for it.
    pub unclean_exit_record: Option<PathBuf>,
    /// Pre-existing crash records found in the crash dir (panic-hook
    /// output from earlier sessions), newest last.
    pub existing_records: Vec<PathBuf>,
}

/// Scan the crash dir at boot: a stale `session.marker` means the
/// previous session died WITHOUT a clean exit (and without the panic
/// hook firing — abort/SIGKILL/segfault); convert it into an
/// `unclean-exit` crash record. Also surface any pre-existing crash
/// records. Everything noteworthy is logged at WARN with file paths.
pub fn startup_scan() -> StartupScanReport {
    startup_scan_in(&crash_dir())
}

fn startup_scan_in(dir: &Path) -> StartupScanReport {
    let mut report = StartupScanReport::default();

    // 1. Pre-existing crash records (panic-hook output from past runs).
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut records: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".json"))
            })
            .collect();
        records.sort();
        report.existing_records = records;
    }

    // 2. Stale session marker ⇒ previous process died uncleanly.
    let marker = marker_path_in(dir);
    if marker.exists() {
        let stale: Option<SessionMarker> = std::fs::read(&marker)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let (prev_state, prev_pid, prev_started) = stale
            .map(|m| (m.last_state, m.pid.to_string(), m.started_at))
            .unwrap_or_else(|| {
                (
                    "<marker unreadable>".to_string(),
                    "<unknown>".to_string(),
                    "<unknown>".to_string(),
                )
            });
        let record = CrashRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_version: APP_VERSION.to_string(),
            kind: "unclean-exit".to_string(),
            thread: "<unknown>".to_string(),
            message: format!(
                "previous session (pid {prev_pid}, started {prev_started}) died without a \
                 clean exit and without the panic hook firing — killed, aborted, or \
                 segfaulted; last known state: {prev_state}"
            ),
            location: "<unknown>".to_string(),
            backtrace: "<unavailable — process died without unwinding>".to_string(),
            last_state: prev_state,
        };
        match write_crash_record(dir, &record) {
            Ok(path) => {
                tracing::warn!(
                    "crash-telemetry: UNCLEAN EXIT detected — previous session left a stale \
                     marker at {}; unclean-exit record written to {}",
                    marker.display(),
                    path.display(),
                );
                report.unclean_exit_record = Some(path);
            }
            Err(e) => {
                tracing::warn!(
                    "crash-telemetry: stale marker at {} but failed to write unclean-exit \
                     record: {e}",
                    marker.display(),
                );
            }
        }
        let _ = std::fs::remove_file(&marker);
    }

    if !report.existing_records.is_empty() {
        tracing::warn!(
            "crash-telemetry: {} crash record(s) from previous sessions in {} — newest: {}",
            report.existing_records.len(),
            dir.display(),
            report
                .existing_records
                .last()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    }

    report
}

// =========================================================================
// Rotating file log (tracing MakeWriter)
// =========================================================================

struct LogInner {
    file: std::fs::File,
    path: PathBuf,
    len: u64,
    max_len: u64,
}

/// Size-rotating append writer for the tracing subscriber. Keeps the
/// current file plus one rotation (`citrate-gui.log` → `citrate-gui.log.1`).
/// Clone-able; all clones share the same file handle behind a mutex —
/// tracing writes one formatted event per `write_all`, so interleaving
/// is line-atomic in practice.
#[derive(Clone)]
pub struct FileLogWriter {
    inner: Arc<Mutex<LogInner>>,
}

impl FileLogWriter {
    pub fn new(path: PathBuf, max_len: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict_dir_perms(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // NAT-B-005: the log can carry user-identifying breadcrumbs — keep
        // it owner-only, not the 0644 default the umask would leave.
        restrict_file_perms(&path);
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(LogInner {
                file,
                path,
                len,
                max_len,
            })),
        })
    }

    fn rotate(inner: &mut LogInner) -> io::Result<()> {
        inner.file.flush()?;
        let mut rotated = inner.path.clone().into_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&inner.path, &rotated)?;
        inner.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inner.path)?;
        // NAT-B-005: re-created log after rotation stays owner-only.
        restrict_file_perms(&inner.path);
        inner.len = 0;
        Ok(())
    }
}

impl io::Write for FileLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer poisoned"))?;
        if inner.len.saturating_add(buf.len() as u64) > inner.max_len {
            // Rotation failure must not kill logging — keep appending.
            let _ = Self::rotate(&mut inner);
        }
        let n = inner.file.write(buf)?;
        inner.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer poisoned"))?
            .file
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileLogWriter {
    type Writer = FileLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// ENCRYPT-S1 WP-4: redacting wrapper around [`FileLogWriter`]. Every
/// buffer headed for the on-disk log passes through
/// [`redact_for_disk`] first. The stdout/stderr tracing layer is NOT
/// wrapped — terminal output stays verbatim; disk is the boundary.
///
/// tracing's fmt layer hands the writer one whole formatted event per
/// `write` call, so hex/balance tokens are never split across buffers
/// in practice.
#[derive(Clone)]
pub struct RedactingFileLogWriter {
    inner: FileLogWriter,
}

impl RedactingFileLogWriter {
    pub fn new(inner: FileLogWriter) -> Self {
        Self { inner }
    }
}

impl io::Write for RedactingFileLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let redacted = redact_for_disk(&text);
        self.inner.write_all(redacted.as_bytes())?;
        // Report the caller's bytes as consumed — the transformed
        // length may differ and must not confuse the fmt layer.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingFileLogWriter {
    type Writer = RedactingFileLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Build the production rotating file-log writer
/// (`~/.local/share/citrate-gui/logs/citrate-gui.log`, 5 MB rotation),
/// wrapped in the WP-4 redaction pass. `None` (with a stderr note) if
/// the log dir is unusable — file logging is diagnostics, never a
/// startup blocker.
pub fn file_log_writer() -> Option<RedactingFileLogWriter> {
    let path = logs_dir().join("citrate-gui.log");
    match FileLogWriter::new(path.clone(), MAX_LOG_BYTES) {
        Ok(w) => Some(RedactingFileLogWriter::new(w)),
        Err(e) => {
            eprintln!(
                "[crash-telemetry] file log disabled ({} unusable): {e}",
                path.display()
            );
            None
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Fresh unique temp dir per test — no tempfile dependency.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "citrate-crash-telemetry-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_records(dir: &Path) -> Vec<CrashRecord> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".json"))
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect()
    }

    /// WP-A1 AC part 1 (direct path): the record-writing fn produces a
    /// parseable JSON crash record carrying message, backtrace,
    /// timestamp, version, and last state.
    #[test]
    fn crash_record_written_with_message_and_backtrace() {
        let dir = temp_dir("direct");
        let record = CrashRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_version: APP_VERSION.to_string(),
            kind: "panic".to_string(),
            thread: "test-thread".to_string(),
            message: "boom: direct write".to_string(),
            location: "src/somewhere.rs:1:1".to_string(),
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
            last_state: "unit-test".to_string(),
        };
        let path = write_crash_record(&dir, &record).expect("write");
        assert!(path.exists());
        let read: CrashRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).expect("parse");
        assert_eq!(read.message, "boom: direct write");
        assert_eq!(read.app_version, APP_VERSION);
        assert_eq!(read.last_state, "unit-test");
        assert!(!read.backtrace.is_empty(), "backtrace must be captured");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WP-A1 AC part 1 (full path, injected panic): re-exec this test
    /// binary as a child with `CITRATE_CRASH_HOOK_CHILD=1`; the child
    /// installs the REAL hook, records a last state, and panics. The
    /// parent asserts a crash record landed in the redirected crash dir
    /// with the panic message + a non-empty backtrace.
    ///
    /// Data source: the crash-record file written by the child process.
    #[test]
    fn injected_panic_child_process_writes_crash_record() {
        if std::env::var("CITRATE_CRASH_HOOK_CHILD").is_ok() {
            // ---- child branch: run the production panic path ----
            install_panic_hook();
            set_last_state("injected-panic-test (node-start telemetry)");
            panic!("injected test panic: WP-A1 crash telemetry");
        }

        let data_dir = temp_dir("child");
        let exe = std::env::current_exe().expect("current_exe");
        let output = std::process::Command::new(exe)
            .arg("crash_telemetry::tests::injected_panic_child_process_writes_crash_record")
            .arg("--exact")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("CITRATE_CRASH_HOOK_CHILD", "1")
            .env(DATA_DIR_ENV, &data_dir)
            .output()
            .expect("spawn child test process");
        assert!(
            !output.status.success(),
            "child must fail (it panicked); stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let crash_dir = data_dir.join("crash");
        let records = read_records(&crash_dir);
        assert_eq!(
            records.len(),
            1,
            "exactly one crash record expected in {}",
            crash_dir.display()
        );
        let rec = &records[0];
        assert_eq!(rec.kind, "panic");
        assert!(
            rec.message.contains("injected test panic: WP-A1 crash telemetry"),
            "message: {}",
            rec.message
        );
        assert!(
            rec.backtrace.contains("crash_telemetry")
                || rec.backtrace.lines().count() > 3,
            "backtrace must be substantive, got: {}",
            rec.backtrace
        );
        assert_eq!(
            rec.last_state,
            "injected-panic-test (node-start telemetry)"
        );
        assert_eq!(rec.app_version, APP_VERSION);
        // The chained previous hook must still have printed the panic
        // (libtest routes it to stdout or stderr depending on capture).
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            combined.contains("injected test panic: WP-A1 crash telemetry"),
            "panic must still reach the terminal, got: {combined}"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// WP-A1 abort/SIGKILL detection: a stale session marker at boot is
    /// converted by `startup_scan_in` into an `unclean-exit` crash record
    /// carrying the previous session's last known state; the marker is
    /// consumed. A clean exit (marker removed) leaves nothing behind.
    #[test]
    fn stale_session_marker_becomes_unclean_exit_record() {
        let dir = temp_dir("marker");

        // Simulate a previous session: marker written, state recorded,
        // then the process "dies" without removing it.
        let marker = marker_path_in(&dir);
        write_marker_file(&marker, "node-start (settings)").unwrap();
        assert!(marker.exists());

        // Next launch:
        let report = startup_scan_in(&dir);
        let rec_path = report
            .unclean_exit_record
            .expect("stale marker must yield an unclean-exit record");
        let rec: CrashRecord =
            serde_json::from_slice(&std::fs::read(&rec_path).unwrap()).unwrap();
        assert_eq!(rec.kind, "unclean-exit");
        assert_eq!(rec.last_state, "node-start (settings)");
        assert!(rec.message.contains("died without a clean exit"));
        assert!(!marker.exists(), "stale marker must be consumed");

        // And the scan after a CLEAN previous exit reports nothing new.
        write_marker_file(&marker, "session-start").unwrap();
        std::fs::remove_file(&marker).unwrap(); // clean exit
        let report2 = startup_scan_in(&dir);
        assert!(report2.unclean_exit_record.is_none());
        // The earlier unclean-exit record is still surfaced as existing.
        assert_eq!(report2.existing_records, vec![rec_path]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ENCRYPT-S1 WP-4 (inventory A7/A8): disk redaction ───────────

    /// Synthetic 0x-40-hex address, constructed (not a literal) so the
    /// marketplace_client "no address literals in src/" tripwire test
    /// doesn't fire on a redaction fixture. `0xd8dA6BF2…` × 5 → first-6
    /// is "0xd8dA", last-4 is "6BF2".
    fn test_addr() -> String {
        format!("0x{}", "d8dA6BF2".repeat(5))
    }

    const HASH: &str = "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7";

    /// Addresses and hashes are truncated to first-6…last-4; the full
    /// identifier never survives.
    #[test]
    fn redaction_truncates_addresses_and_hashes() {
        let addr = test_addr();
        let out = redact_for_disk(&format!("sending from {addr} tx {HASH} done"));
        assert!(out.contains("0xd8dA…6BF2"), "address truncated, got: {out}");
        assert!(out.contains("9b71d2…3ca7"), "hash truncated, got: {out}");
        assert!(!out.contains(&addr), "full address must not survive");
        assert!(!out.contains(HASH), "full hash must not survive");
        // 0x-prefixed 64-hex (tx hash as usually printed) too.
        let out2 = redact_for_disk(&format!("tx 0x{HASH} pending"));
        assert!(out2.contains("0x9b71…3ca7"), "0x-hash truncated, got: {out2}");
        assert!(!out2.contains(HASH));
        // Short hex (code pointers like backtrace frame addresses)
        // is NOT touched.
        let frames = "at 0x7f3a9c04d123 in start_thread";
        assert_eq!(redact_for_disk(frames), frames);
    }

    /// balance/amount-adjacent values become `<redacted>` (decimal,
    /// separators, JSON-style, and hex values alike); the key survives.
    #[test]
    fn redaction_scrubs_balance_and_amount_values() {
        let out = redact_for_disk("wallet-view balance=1234.56 SALT");
        assert_eq!(out, "wallet-view balance=<redacted> SALT");
        let out = redact_for_disk(r#"{"amount": 3_000_000}"#);
        assert!(out.contains(r#""amount": <redacted>"#), "got: {out}");
        assert!(!out.contains("3_000_000"));
        let out = redact_for_disk("total_amount: 42 pending_balance=7");
        assert!(out.contains("total_amount: <redacted>"), "got: {out}");
        assert!(out.contains("pending_balance=<redacted>"), "got: {out}");
        let out = redact_for_disk(&format!("balance=0x{HASH}"));
        assert_eq!(out, "balance=<redacted>", "hex balance value fully scrubbed");
        // Redaction is idempotent — re-scrubbing scrubbed text is a no-op.
        let addr = test_addr();
        let once = redact_for_disk(&format!("send {addr} balance=9"));
        assert_eq!(redact_for_disk(&once), once);
    }

    /// The session marker is redacted on disk AND still parses as JSON
    /// (redaction happens on field values before serialization).
    #[test]
    fn redacted_marker_survives_json_round_trip() {
        let dir = temp_dir("redact-marker");
        let marker = marker_path_in(&dir);
        let addr = test_addr();
        let state = format!("send-flow to {addr} balance=555.5");
        write_marker_file(&marker, &state).unwrap();

        let raw = std::fs::read(&marker).unwrap();
        assert!(
            !raw.windows(addr.len()).any(|w| w == addr.as_bytes()),
            "full address must not reach disk"
        );
        let parsed: SessionMarker = serde_json::from_slice(&raw).expect("marker stays valid JSON");
        assert!(parsed.last_state.contains("0xd8dA…6BF2"), "got: {}", parsed.last_state);
        assert!(parsed.last_state.contains("balance=<redacted>"), "got: {}", parsed.last_state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crash records scrub message + last_state at the disk boundary
    /// but keep the backtrace verbatim (symbols are the forensics).
    #[test]
    fn crash_record_on_disk_is_redacted_but_backtrace_kept() {
        let dir = temp_dir("redact-record");
        let addr = test_addr();
        let backtrace = "0: citrate_native::send_flow::submit\n   at src/main.rs:100";
        let record = CrashRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_version: APP_VERSION.to_string(),
            kind: "panic".to_string(),
            thread: "main".to_string(),
            message: format!("send failed for {addr}: amount=12.5 rejected"),
            location: "src/main.rs:100:5".to_string(),
            backtrace: backtrace.to_string(),
            last_state: format!("send-confirm ({addr}, balance=99)"),
        };
        let path = write_crash_record(&dir, &record).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(addr.len()).any(|w| w == addr.as_bytes()),
            "full address must not reach disk"
        );
        let read: CrashRecord = serde_json::from_slice(&raw).unwrap();
        assert!(read.message.contains("0xd8dA…6BF2"), "got: {}", read.message);
        assert!(read.message.contains("amount=<redacted>"), "got: {}", read.message);
        assert!(read.last_state.contains("balance=<redacted>"), "got: {}", read.last_state);
        assert_eq!(read.backtrace, backtrace, "backtrace symbols kept verbatim");
        assert_eq!(read.location, "src/main.rs:100:5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The redacting MakeWriter scrubs what lands in the file log.
    #[test]
    fn redacting_log_writer_scrubs_file_output() {
        let dir = temp_dir("redact-log");
        let path = dir.join("citrate-gui.log");
        let inner = FileLogWriter::new(path.clone(), 4096).unwrap();
        let mut w = RedactingFileLogWriter::new(inner);
        let addr = test_addr();
        let line = format!("INFO wallet balance=42.7 owner {addr}\n");
        w.write_all(line.as_bytes()).unwrap();
        w.flush().unwrap();
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(logged.contains("balance=<redacted>"), "got: {logged}");
        assert!(logged.contains("0xd8dA…6BF2"), "got: {logged}");
        assert!(!logged.contains(&addr), "full address must not reach the log file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// NAT-B-005: mnemonics, passwords, bearer tokens, and Argon2 PHC
    /// strings must NOT survive the disk-redaction boundary — neither
    /// verbatim nor truncated. Pre-fix the deny-list covered only
    /// balance/amount and long-hex, so 5 of 6 secret classes were written
    /// to disk in cleartext. RED at parent.
    #[test]
    fn redaction_scrubs_mnemonics_passwords_and_tokens() {
        // Canonical 12-word BIP-39 phrase (all-zero entropy).
        let mnemonic =
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let out = redact_for_disk(mnemonic);
        assert!(!out.contains("abandon"), "bare mnemonic run must not survive: {out}");
        assert!(!out.contains("about"), "mnemonic tail must not survive: {out}");

        // A labeled mnemonic (the onboarding-verify shape) also goes.
        let out = redact_for_disk("expected word #7 'shrimp' in mnemonic=\"shrimp legal winner\"");
        assert!(!out.contains("shrimp legal winner"), "labeled mnemonic must not survive: {out}");

        // Keystore password by key.
        let out = redact_for_disk("unlock failed password=Hunter2-Sekret!");
        assert!(!out.contains("Hunter2-Sekret"), "password value must not survive: {out}");
        assert!(out.contains("password="), "the key label is kept for context: {out}");

        // Bearer / API token.
        let out = redact_for_disk("auth token: bk_live_0123456789ABCDEFdeadbeef");
        assert!(!out.contains("bk_live_0123456789ABCDEFdeadbeef"), "token must not survive: {out}");

        // Argon2 PHC string.
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
        let out = redact_for_disk(&format!("keystore hash {phc} loaded"));
        assert!(!out.contains("c29tZXNhbHQ"), "PHC salt must not survive: {out}");
        assert!(!out.contains("RdescudvJCsgt3ub"), "PHC hash must not survive: {out}");
    }

    /// NAT-B-005: files this module writes must be owner-only (0600) on
    /// Unix — crash record, session marker, and the rotating log. Pre-fix
    /// every one was created 0644 (world-readable). RED at parent.
    #[cfg(unix)]
    #[test]
    fn disk_artifacts_are_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");

        // Crash record.
        let record = CrashRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            app_version: APP_VERSION.to_string(),
            kind: "panic".to_string(),
            thread: "main".to_string(),
            message: "boom".to_string(),
            location: "src/main.rs:1:1".to_string(),
            backtrace: String::new(),
            last_state: "state".to_string(),
        };
        let rec_path = write_crash_record(&dir, &record).unwrap();
        let mode = std::fs::metadata(&rec_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "crash record must be 0600, got {mode:o}");

        // Session marker.
        let marker = marker_path_in(&dir);
        write_marker_file(&marker, "session-start").unwrap();
        let mode = std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "session marker must be 0600, got {mode:o}");

        // Rotating log.
        let log = dir.join("citrate-gui.log");
        let mut w = FileLogWriter::new(log.clone(), 4096).unwrap();
        w.write_all(b"line\n").unwrap();
        w.flush().unwrap();
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file must be 0600, got {mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rotating file log rotates at max_len and keeps exactly one
    /// prior generation.
    #[test]
    fn file_log_rotates_at_size_limit() {
        let dir = temp_dir("logrot");
        let path = dir.join("citrate-gui.log");
        let mut w = FileLogWriter::new(path.clone(), 256).unwrap();
        let line = vec![b'x'; 64];
        for _ in 0..8 {
            w.write_all(&line).unwrap();
        }
        w.flush().unwrap();
        let rotated = dir.join("citrate-gui.log.1");
        assert!(rotated.exists(), "rotation must produce {}", rotated.display());
        assert!(path.exists(), "current log must be re-created");
        assert!(
            std::fs::metadata(&path).unwrap().len() <= 256,
            "current log stays within the limit after rotation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
