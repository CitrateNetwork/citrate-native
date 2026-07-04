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
//! Data source (Rule 11): the crash-record files themselves + the
//! injected-panic child-process test in this module's test suite.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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
pub fn write_crash_record(dir: &Path, record: &CrashRecord) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = RECORD_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("crash-{millis}-{seq}.json"));
    let json = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)?;
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
        last_state: state.to_string(),
        updated_at: now,
    };
    let json = serde_json::to_vec_pretty(&marker)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
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
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
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

/// Build the production rotating file-log writer
/// (`~/.local/share/citrate-gui/logs/citrate-gui.log`, 5 MB rotation).
/// `None` (with a stderr note) if the log dir is unusable — file logging
/// is diagnostics, never a startup blocker.
pub fn file_log_writer() -> Option<FileLogWriter> {
    let path = logs_dir().join("citrate-gui.log");
    match FileLogWriter::new(path.clone(), MAX_LOG_BYTES) {
        Ok(w) => Some(w),
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
