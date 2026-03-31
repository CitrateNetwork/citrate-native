//! Terminal service — PTY session management with VTE-based ANSI parsing.
//!
//! Data source: OS pseudo-terminal (PTY) via `portable-pty` for real backends,
//! in-memory ring buffer for test backends. VTE state machine parses ANSI/VT100
//! escape sequences into a cell grid for rendering.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::view_models::ide_view_models::{
    TerminalCell, TerminalConfig, TerminalGrid, TerminalSessionInfo,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Backend trait for raw PTY I/O. The VTE parsing and cell grid management
/// happen in `TerminalService`, not in the backend. The backend only handles
/// spawning, reading bytes, writing bytes, resizing, and closing.
#[async_trait::async_trait]
pub trait TerminalBackend: Send + Sync {
    /// Spawn a new PTY session. Returns the session ID.
    async fn create_session(
        &self,
        session_id: &str,
        config: &TerminalConfig,
    ) -> Result<(), AppError>;

    /// Write raw bytes to a session's stdin.
    async fn write_input(&self, session_id: &str, data: &[u8]) -> Result<(), AppError>;

    /// Read available bytes from a session's stdout (non-blocking).
    /// Returns an empty vec if nothing is available.
    async fn read_output(&self, session_id: &str) -> Result<Vec<u8>, AppError>;

    /// Resize the PTY for a session.
    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), AppError>;

    /// Close and clean up a session. Returns `Ok(())` even if already closed.
    async fn close_session(&self, session_id: &str) -> Result<(), AppError>;

    /// Return the list of session IDs that are currently alive.
    async fn active_session_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// Real PTY backend
// ---------------------------------------------------------------------------

/// Production backend using `portable_pty` for OS pseudo-terminals.
///
/// Uses `tokio::sync::Mutex` instead of `RwLock` because `MasterPty`,
/// `dyn Write`, and `dyn Read` are `Send` but not `Sync`.
pub struct PtyTerminalBackend {
    sessions: tokio::sync::Mutex<HashMap<String, PtySession>>,
}

struct PtySession {
    writer: Box<dyn std::io::Write + Send>,
    reader: Box<dyn std::io::Read + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    #[allow(dead_code)]
    shell: String,
}

impl PtyTerminalBackend {
    pub fn new() -> Self {
        Self {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl Default for PtyTerminalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TerminalBackend for PtyTerminalBackend {
    async fn create_session(
        &self,
        session_id: &str,
        config: &TerminalConfig,
    ) -> Result<(), AppError> {
        let pty_system = portable_pty::native_pty_system();

        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: config.rows,
                cols: config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Terminal(format!("Failed to open PTY: {e}")))?;

        let shell = config.shell.clone().unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        });

        let mut cmd = portable_pty::CommandBuilder::new(&shell);

        // Set environment variables
        for (key, val) in &config.env {
            cmd.env(key, val);
        }
        // Always set TERM and COLORTERM
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        if let Some(ref cwd) = config.cwd {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::Terminal(format!("Failed to spawn shell '{shell}': {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::Terminal(format!("Failed to get PTY writer: {e}")))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AppError::Terminal(format!("Failed to clone PTY reader: {e}")))?;

        let session = PtySession {
            writer,
            reader,
            _child: child,
            master: pair.master,
            shell: shell.clone(),
        };

        self.sessions
            .lock()
            .await
            .insert(session_id.to_string(), session);

        Ok(())
    }

    async fn write_input(&self, session_id: &str, data: &[u8]) -> Result<(), AppError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;

        use std::io::Write;
        session
            .writer
            .write_all(data)
            .map_err(|e| AppError::Terminal(format!("Write failed: {e}")))?;
        session
            .writer
            .flush()
            .map_err(|e| AppError::Terminal(format!("Flush failed: {e}")))?;
        Ok(())
    }

    async fn read_output(&self, session_id: &str) -> Result<Vec<u8>, AppError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;

        let mut buf = vec![0u8; 4096];
        // Use non-blocking read: set_non_blocking is not portable, so we
        // attempt a read and handle WouldBlock gracefully.
        use std::io::Read;
        match session.reader.read(&mut buf) {
            Ok(0) => Ok(Vec::new()),
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(AppError::Terminal(format!("Read failed: {e}"))),
        }
    }

    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), AppError> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;

        session
            .master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Terminal(format!("Resize failed: {e}")))?;
        Ok(())
    }

    async fn close_session(&self, session_id: &str) -> Result<(), AppError> {
        let _ = self.sessions.lock().await.remove(session_id);
        Ok(())
    }

    async fn active_session_ids(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Test-only in-memory backend
// ---------------------------------------------------------------------------

/// Test-only in-memory backend. Not available in release builds.
#[cfg(test)]
pub struct InMemoryTerminalBackend {
    sessions: RwLock<HashMap<String, TestSession>>,
}

#[cfg(test)]
struct TestSession {
    input_log: Vec<u8>,
    output_buffer: Vec<u8>,
    cols: u16,
    rows: u16,
    _shell: String,
}

#[cfg(test)]
impl Default for InMemoryTerminalBackend {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
impl InMemoryTerminalBackend {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Inject output bytes that will be returned by the next `read_output` call.
    pub async fn inject_output(&self, session_id: &str, data: &[u8]) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(session_id) {
            session.output_buffer.extend_from_slice(data);
        }
    }

    /// Get all bytes written to the session's stdin so far.
    pub async fn get_input_log(&self, session_id: &str) -> Vec<u8> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|s| s.input_log.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl TerminalBackend for InMemoryTerminalBackend {
    async fn create_session(
        &self,
        session_id: &str,
        config: &TerminalConfig,
    ) -> Result<(), AppError> {
        let shell = config
            .shell
            .clone()
            .unwrap_or_else(|| "/bin/sh".to_string());
        self.sessions.write().await.insert(
            session_id.to_string(),
            TestSession {
                input_log: Vec::new(),
                output_buffer: Vec::new(),
                cols: config.cols,
                rows: config.rows,
                _shell: shell,
            },
        );
        Ok(())
    }

    async fn write_input(&self, session_id: &str, data: &[u8]) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        session.input_log.extend_from_slice(data);
        Ok(())
    }

    async fn read_output(&self, session_id: &str) -> Result<Vec<u8>, AppError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        let data = std::mem::take(&mut session.output_buffer);
        Ok(data)
    }

    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        session.cols = cols;
        session.rows = rows;
        Ok(())
    }

    async fn close_session(&self, session_id: &str) -> Result<(), AppError> {
        let _ = self.sessions.write().await.remove(session_id);
        Ok(())
    }

    async fn active_session_ids(&self) -> Vec<String> {
        self.sessions.read().await.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// VTE terminal state — parses ANSI escape sequences into a cell grid
// ---------------------------------------------------------------------------

/// Default foreground color (matches TerminalCell default).
const DEFAULT_FG: &str = "#f9fafb";
/// Default background color (matches TerminalCell default).
const DEFAULT_BG: &str = "#0a0a1a";

/// Internal terminal emulator state. Implements `vte::Perform` to receive
/// parsed ANSI/VT100 events from the VTE state machine.
struct TerminalState {
    cells: Vec<Vec<TerminalCell>>,
    cursor_row: usize,
    cursor_col: usize,
    rows: u16,
    cols: u16,
    fg_color: String,
    bg_color: String,
    bold: bool,
    underline: bool,
    cursor_visible: bool,
    /// VTE parser instance — owned by the state so we can feed bytes.
    parser: vte::Parser,
}

impl TerminalState {
    fn new(rows: u16, cols: u16) -> Self {
        let cells = (0..rows as usize)
            .map(|_| {
                (0..cols as usize)
                    .map(|_| TerminalCell::default())
                    .collect()
            })
            .collect();
        Self {
            cells,
            cursor_row: 0,
            cursor_col: 0,
            rows,
            cols,
            fg_color: DEFAULT_FG.to_string(),
            bg_color: DEFAULT_BG.to_string(),
            bold: false,
            underline: false,
            cursor_visible: true,
            parser: vte::Parser::new(),
        }
    }

    /// Feed raw bytes through the VTE parser. This will call back into the
    /// `vte::Perform` implementation on a helper struct.
    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            // We must use an indirection because `vte::Parser::advance` takes
            // `&mut self` for the parser and `&mut impl Perform` for the performer.
            // We cannot pass `&mut self` as both, so we use a thin wrapper that
            // borrows only the grid-related fields.
            let mut performer = TerminalPerformer {
                cells: &mut self.cells,
                cursor_row: &mut self.cursor_row,
                cursor_col: &mut self.cursor_col,
                rows: self.rows,
                cols: self.cols,
                fg_color: &mut self.fg_color,
                bg_color: &mut self.bg_color,
                bold: &mut self.bold,
                underline: &mut self.underline,
                cursor_visible: &mut self.cursor_visible,
            };
            self.parser.advance(&mut performer, byte);
        }
    }

    fn to_grid(&self) -> TerminalGrid {
        TerminalGrid {
            cells: self.cells.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible: self.cursor_visible,
            rows: self.rows,
            cols: self.cols,
        }
    }

    /// Resize the grid. Content is preserved where possible.
    fn resize(&mut self, new_rows: u16, new_cols: u16) {
        let nr = new_rows as usize;
        let nc = new_cols as usize;

        // Resize each existing row
        for row in &mut self.cells {
            row.resize_with(nc, TerminalCell::default);
        }
        // Add or remove rows
        self.cells
            .resize_with(nr, || vec![TerminalCell::default(); nc]);

        self.rows = new_rows;
        self.cols = new_cols;

        // Clamp cursor
        if self.cursor_row >= nr {
            self.cursor_row = nr.saturating_sub(1);
        }
        if self.cursor_col >= nc {
            self.cursor_col = nc.saturating_sub(1);
        }
    }
}

/// Thin borrow wrapper that implements `vte::Perform`.
struct TerminalPerformer<'a> {
    cells: &'a mut Vec<Vec<TerminalCell>>,
    cursor_row: &'a mut usize,
    cursor_col: &'a mut usize,
    rows: u16,
    cols: u16,
    fg_color: &'a mut String,
    bg_color: &'a mut String,
    bold: &'a mut bool,
    underline: &'a mut bool,
    cursor_visible: &'a mut bool,
}

impl<'a> TerminalPerformer<'a> {
    /// Scroll the grid up by one line: shift all rows up and clear the bottom.
    fn scroll_up(&mut self) {
        let nc = self.cols as usize;
        if self.cells.len() > 1 {
            self.cells.remove(0);
        }
        self.cells.push(vec![TerminalCell::default(); nc]);
    }

    /// Advance cursor to the next line, scrolling if at the bottom.
    fn newline(&mut self) {
        if *self.cursor_row + 1 >= self.rows as usize {
            self.scroll_up();
            // cursor_row stays at the last row
        } else {
            *self.cursor_row += 1;
        }
    }

    /// Put a character at the current cursor position, respecting wrapping.
    fn put_char(&mut self, ch: char) {
        // If cursor is at the right edge, wrap to the next line first
        if *self.cursor_col >= self.cols as usize {
            *self.cursor_col = 0;
            self.newline();
        }

        let r = *self.cursor_row;
        let c = *self.cursor_col;
        if r < self.cells.len() && c < self.cells[r].len() {
            self.cells[r][c] = TerminalCell {
                character: ch,
                fg_color: self.fg_color.clone(),
                bg_color: self.bg_color.clone(),
                bold: *self.bold,
                underline: *self.underline,
            };
        }
        *self.cursor_col += 1;
    }

    /// Parse SGR (Select Graphic Rendition) parameters.
    fn apply_sgr(&mut self, params: &[&[u16]]) {
        let mut i = 0;
        while i < params.len() {
            let p = if params[i].is_empty() {
                0
            } else {
                params[i][0]
            };
            match p {
                0 => {
                    // Reset
                    *self.fg_color = DEFAULT_FG.to_string();
                    *self.bg_color = DEFAULT_BG.to_string();
                    *self.bold = false;
                    *self.underline = false;
                }
                1 => *self.bold = true,
                4 => *self.underline = true,
                22 => *self.bold = false,
                24 => *self.underline = false,
                // Standard foreground colors (30-37)
                30 => *self.fg_color = "#000000".to_string(),
                31 => *self.fg_color = "#cc0000".to_string(),
                32 => *self.fg_color = "#4e9a06".to_string(),
                33 => *self.fg_color = "#c4a000".to_string(),
                34 => *self.fg_color = "#3465a4".to_string(),
                35 => *self.fg_color = "#75507b".to_string(),
                36 => *self.fg_color = "#06989a".to_string(),
                37 => *self.fg_color = "#d3d7cf".to_string(),
                39 => *self.fg_color = DEFAULT_FG.to_string(),
                // Standard background colors (40-47)
                40 => *self.bg_color = "#000000".to_string(),
                41 => *self.bg_color = "#cc0000".to_string(),
                42 => *self.bg_color = "#4e9a06".to_string(),
                43 => *self.bg_color = "#c4a000".to_string(),
                44 => *self.bg_color = "#3465a4".to_string(),
                45 => *self.bg_color = "#75507b".to_string(),
                46 => *self.bg_color = "#06989a".to_string(),
                47 => *self.bg_color = "#d3d7cf".to_string(),
                49 => *self.bg_color = DEFAULT_BG.to_string(),
                // Bright foreground colors (90-97)
                90 => *self.fg_color = "#555753".to_string(),
                91 => *self.fg_color = "#ef2929".to_string(),
                92 => *self.fg_color = "#8ae234".to_string(),
                93 => *self.fg_color = "#fce94f".to_string(),
                94 => *self.fg_color = "#729fcf".to_string(),
                95 => *self.fg_color = "#ad7fa8".to_string(),
                96 => *self.fg_color = "#34e2e2".to_string(),
                97 => *self.fg_color = "#eeeeec".to_string(),
                // 256-color mode: 38;5;N (fg), 48;5;N (bg)
                38 => {
                    if i + 1 < params.len() {
                        let sub = if params[i + 1].is_empty() {
                            0
                        } else {
                            params[i + 1][0]
                        };
                        if sub == 5 && i + 2 < params.len() {
                            let color_idx = if params[i + 2].is_empty() {
                                0
                            } else {
                                params[i + 2][0]
                            };
                            *self.fg_color = color_256_to_hex(color_idx);
                            i += 2; // skip the two consumed sub-params
                        }
                    }
                }
                48 => {
                    if i + 1 < params.len() {
                        let sub = if params[i + 1].is_empty() {
                            0
                        } else {
                            params[i + 1][0]
                        };
                        if sub == 5 && i + 2 < params.len() {
                            let color_idx = if params[i + 2].is_empty() {
                                0
                            } else {
                                params[i + 2][0]
                            };
                            *self.bg_color = color_256_to_hex(color_idx);
                            i += 2;
                        }
                    }
                }
                _ => {} // Unhandled SGR codes — ignore
            }
            i += 1;
        }
    }
}

impl<'a> vte::Perform for TerminalPerformer<'a> {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Newline (LF)
            b'\n' => {
                self.newline();
            }
            // Carriage return
            b'\r' => {
                *self.cursor_col = 0;
            }
            // Backspace
            0x08 => {
                *self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            // Tab — advance to next 8-column tab stop
            b'\t' => {
                let next_tab = (*self.cursor_col + 8) & !7;
                *self.cursor_col = next_tab.min(self.cols as usize - 1);
            }
            // Bell (0x07), other C0 controls — ignore
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // Collect params into a Vec<&[u16]> for easier indexing
        let param_list: Vec<&[u16]> = params.iter().collect();

        let p0 = if param_list.is_empty() || param_list[0].is_empty() {
            0
        } else {
            param_list[0][0]
        };
        // Default of 1 for movement commands (0 means 1)
        let n = if p0 == 0 { 1 } else { p0 as usize };

        match action {
            // Cursor Up
            'A' => {
                *self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            // Cursor Down
            'B' => {
                let max_row = (self.rows as usize).saturating_sub(1);
                *self.cursor_row = (*self.cursor_row + n).min(max_row);
            }
            // Cursor Forward
            'C' => {
                let max_col = (self.cols as usize).saturating_sub(1);
                *self.cursor_col = (*self.cursor_col + n).min(max_col);
            }
            // Cursor Back
            'D' => {
                *self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            // Cursor Position (CUP) — CSI row;col H
            'H' | 'f' => {
                let row = if p0 == 0 { 1 } else { p0 as usize };
                let col = if param_list.len() >= 2 && !param_list[1].is_empty() {
                    let c = param_list[1][0];
                    if c == 0 { 1 } else { c as usize }
                } else {
                    1
                };
                *self.cursor_row =
                    (row.saturating_sub(1)).min((self.rows as usize).saturating_sub(1));
                *self.cursor_col =
                    (col.saturating_sub(1)).min((self.cols as usize).saturating_sub(1));
            }
            // Erase in Display
            'J' => {
                let mode = p0;
                let nr = self.rows as usize;
                let nc = self.cols as usize;
                match mode {
                    0 => {
                        // Erase from cursor to end of screen
                        // Clear rest of current line
                        for c in *self.cursor_col..nc {
                            if *self.cursor_row < self.cells.len()
                                && c < self.cells[*self.cursor_row].len()
                            {
                                self.cells[*self.cursor_row][c] = TerminalCell::default();
                            }
                        }
                        // Clear remaining lines
                        for r in (*self.cursor_row + 1)..nr {
                            if r < self.cells.len() {
                                for c in 0..nc {
                                    if c < self.cells[r].len() {
                                        self.cells[r][c] = TerminalCell::default();
                                    }
                                }
                            }
                        }
                    }
                    1 => {
                        // Erase from start of screen to cursor
                        for r in 0..*self.cursor_row {
                            if r < self.cells.len() {
                                for c in 0..nc {
                                    if c < self.cells[r].len() {
                                        self.cells[r][c] = TerminalCell::default();
                                    }
                                }
                            }
                        }
                        // Clear current line up to and including cursor
                        for c in 0..=*self.cursor_col {
                            if *self.cursor_row < self.cells.len()
                                && c < self.cells[*self.cursor_row].len()
                            {
                                self.cells[*self.cursor_row][c] = TerminalCell::default();
                            }
                        }
                    }
                    2 | 3 => {
                        // Erase entire display
                        for r in 0..nr {
                            if r < self.cells.len() {
                                for c in 0..nc {
                                    if c < self.cells[r].len() {
                                        self.cells[r][c] = TerminalCell::default();
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Erase in Line
            'K' => {
                let mode = p0;
                let nc = self.cols as usize;
                let r = *self.cursor_row;
                if r < self.cells.len() {
                    match mode {
                        0 => {
                            // Erase from cursor to end of line
                            for c in *self.cursor_col..nc {
                                if c < self.cells[r].len() {
                                    self.cells[r][c] = TerminalCell::default();
                                }
                            }
                        }
                        1 => {
                            // Erase from start of line to cursor
                            for c in 0..=*self.cursor_col {
                                if c < self.cells[r].len() {
                                    self.cells[r][c] = TerminalCell::default();
                                }
                            }
                        }
                        2 => {
                            // Erase entire line
                            for c in 0..nc {
                                if c < self.cells[r].len() {
                                    self.cells[r][c] = TerminalCell::default();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            // SGR (Select Graphic Rendition)
            'm' => {
                self.apply_sgr(&param_list);
            }
            // Show/hide cursor
            'l' | 'h' => {
                // CSI ?25l = hide cursor, CSI ?25h = show cursor
                // The '?' is an intermediate, but vte may pass it differently.
                // We check for p0 == 25 as a simplification.
                if p0 == 25 {
                    *self.cursor_visible = action == 'h';
                }
            }
            _ => {} // Unhandled CSI sequences — ignore
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // Most ESC sequences are handled by VTE internally. We ignore the rest.
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS sequences — not needed for basic terminal emulation
    }

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC sequences (window title, etc.) — currently ignored
    }

    fn put(&mut self, _byte: u8) {
        // DCS data — not needed for basic terminal emulation
    }
}

/// Convert a 256-color index to a hex color string.
fn color_256_to_hex(idx: u16) -> String {
    match idx {
        // Standard colors (0-7)
        0 => "#000000".to_string(),
        1 => "#cc0000".to_string(),
        2 => "#4e9a06".to_string(),
        3 => "#c4a000".to_string(),
        4 => "#3465a4".to_string(),
        5 => "#75507b".to_string(),
        6 => "#06989a".to_string(),
        7 => "#d3d7cf".to_string(),
        // Bright colors (8-15)
        8 => "#555753".to_string(),
        9 => "#ef2929".to_string(),
        10 => "#8ae234".to_string(),
        11 => "#fce94f".to_string(),
        12 => "#729fcf".to_string(),
        13 => "#ad7fa8".to_string(),
        14 => "#34e2e2".to_string(),
        15 => "#eeeeec".to_string(),
        // 216 color cube (16-231): 6x6x6
        16..=231 => {
            let idx = (idx - 16) as u8;
            let r_idx = idx / 36;
            let g_idx = (idx % 36) / 6;
            let b_idx = idx % 6;
            let r = if r_idx == 0 { 0u8 } else { 55 + 40 * r_idx };
            let g = if g_idx == 0 { 0u8 } else { 55 + 40 * g_idx };
            let b = if b_idx == 0 { 0u8 } else { 55 + 40 * b_idx };
            format!("#{r:02x}{g:02x}{b:02x}")
        }
        // Grayscale ramp (232-255)
        232..=255 => {
            let level = 8 + 10 * (idx - 232) as u8;
            format!("#{level:02x}{level:02x}{level:02x}")
        }
        _ => DEFAULT_FG.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Terminal session metadata
// ---------------------------------------------------------------------------

struct SessionMeta {
    title: String,
    shell: String,
    is_active: bool,
    state: TerminalState,
}

// ---------------------------------------------------------------------------
// TerminalService
// ---------------------------------------------------------------------------

/// Terminal service — manages PTY sessions, VTE parsing, and cell grid state.
pub struct TerminalService {
    events: Arc<EventBus>,
    sessions: Arc<RwLock<HashMap<String, SessionMeta>>>,
    backend: Arc<dyn TerminalBackend>,
}

impl TerminalService {
    /// Create with the real PTY backend.
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            backend: Arc::new(PtyTerminalBackend::new()),
        }
    }

    /// Create with an injected backend (for testing or alternative I/O).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn TerminalBackend>) -> Self {
        Self {
            events,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            backend,
        }
    }

    /// Create a new terminal session. Returns the session ID.
    pub async fn create_session(
        &self,
        config: &TerminalConfig,
    ) -> Result<String, AppError> {
        let session_id = uuid::Uuid::new_v4().to_string();

        self.backend
            .create_session(&session_id, config)
            .await?;

        let shell = config
            .shell
            .clone()
            .unwrap_or_else(|| "/bin/sh".to_string());
        let title = format!("Terminal ({})", shell_name(&shell));
        let state = TerminalState::new(config.rows, config.cols);

        let meta = SessionMeta {
            title: title.clone(),
            shell,
            is_active: true,
            state,
        };

        self.sessions
            .write()
            .await
            .insert(session_id.clone(), meta);

        Ok(session_id)
    }

    /// Write input bytes to a session's PTY stdin.
    pub async fn write_input(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), AppError> {
        // Verify session exists in our metadata
        {
            let sessions = self.sessions.read().await;
            if !sessions.contains_key(session_id) {
                return Err(AppError::SessionNotFound(session_id.to_string()));
            }
        }
        self.backend.write_input(session_id, data).await
    }

    /// Read available output from the PTY, parse it through VTE, and update
    /// the cell grid. Returns the number of bytes processed.
    pub async fn poll_output(&self, session_id: &str) -> Result<usize, AppError> {
        let data = self.backend.read_output(session_id).await?;
        if data.is_empty() {
            return Ok(0);
        }

        let len = data.len();

        {
            let mut sessions = self.sessions.write().await;
            let meta = sessions
                .get_mut(session_id)
                .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
            meta.state.feed(&data);
        }

        self.events.publish(AppEvent::TerminalOutputReady {
            session_id: session_id.to_string(),
        });

        Ok(len)
    }

    /// Feed raw bytes directly into the VTE parser for a session.
    /// Useful for testing or injecting synthetic output.
    pub async fn feed_bytes(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;
        let meta = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        meta.state.feed(data);
        drop(sessions);

        self.events.publish(AppEvent::TerminalOutputReady {
            session_id: session_id.to_string(),
        });
        Ok(())
    }

    /// Resize a session's PTY and cell grid.
    pub async fn resize(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), AppError> {
        self.backend.resize(session_id, cols, rows).await?;

        let mut sessions = self.sessions.write().await;
        let meta = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        meta.state.resize(rows, cols);
        Ok(())
    }

    /// Close a session and clean up resources.
    pub async fn close_session(&self, session_id: &str) -> Result<(), AppError> {
        self.backend.close_session(session_id).await?;
        self.sessions.write().await.remove(session_id);

        self.events.publish(AppEvent::TerminalSessionClosed {
            session_id: session_id.to_string(),
        });
        Ok(())
    }

    /// Get the rendered cell grid for a session.
    pub async fn get_grid(&self, session_id: &str) -> Result<TerminalGrid, AppError> {
        let sessions = self.sessions.read().await;
        let meta = sessions
            .get(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        Ok(meta.state.to_grid())
    }

    /// List all active terminal sessions.
    pub async fn list_sessions(&self) -> Vec<TerminalSessionInfo> {
        self.sessions
            .read()
            .await
            .iter()
            .map(|(id, meta)| TerminalSessionInfo {
                session_id: id.clone(),
                title: meta.title.clone(),
                shell: meta.shell.clone(),
                is_active: meta.is_active,
            })
            .collect()
    }

    /// Set the active flag on a session (for tab focus tracking).
    pub async fn set_active(
        &self,
        session_id: &str,
        active: bool,
    ) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;
        let meta = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        meta.is_active = active;
        Ok(())
    }

    /// Set the title of a session.
    pub async fn set_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), AppError> {
        let mut sessions = self.sessions.write().await;
        let meta = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))?;
        meta.title = title.to_string();
        Ok(())
    }
}

/// Extract a short shell name from a full path (e.g. "/bin/zsh" -> "zsh").
fn shell_name(shell_path: &str) -> &str {
    shell_path
        .rsplit('/')
        .next()
        .unwrap_or(shell_path)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a test service with the in-memory backend.
    fn make_service() -> (TerminalService, Arc<InMemoryTerminalBackend>, Arc<EventBus>) {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryTerminalBackend::new());
        let svc = TerminalService::with_backend(events.clone(), backend.clone());
        (svc, backend, events)
    }

    /// Helper: create a session with default config.
    async fn create_default_session(svc: &TerminalService) -> String {
        let config = TerminalConfig::default();
        svc.create_session(&config)
            .await
            .expect("create_session should succeed")
    }

    // -----------------------------------------------------------------------
    // Session lifecycle
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_create_session_returns_uuid() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // UUID v4 format: 8-4-4-4-12 hex characters
        assert_eq!(id.len(), 36, "session ID should be a UUID string");
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
    }

    #[tokio::test]
    async fn test_create_session_appears_in_list() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        let sessions = svc.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, id);
        assert!(sessions[0].is_active);
    }

    #[tokio::test]
    async fn test_close_session_removes_from_list() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.close_session(&id)
            .await
            .expect("close_session should succeed");
        let sessions = svc.list_sessions().await;
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_close_session_publishes_event() {
        let (svc, _, events) = make_service();
        let mut rx = events.subscribe();
        let id = create_default_session(&svc).await;
        svc.close_session(&id)
            .await
            .expect("close_session should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            AppEvent::TerminalSessionClosed { session_id } => {
                assert_eq!(session_id, id);
            }
            _ => panic!("Expected TerminalSessionClosed event"),
        }
    }

    #[tokio::test]
    async fn test_close_nonexistent_session_is_ok() {
        let (svc, _, _) = make_service();
        // Closing a session that does not exist should not error
        let result = svc.close_session("nonexistent-id").await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Input/output
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_write_input_to_session() {
        let (svc, backend, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.write_input(&id, b"ls -la\n")
            .await
            .expect("write_input should succeed");
        let log = backend.get_input_log(&id).await;
        assert_eq!(log, b"ls -la\n");
    }

    #[tokio::test]
    async fn test_write_input_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.write_input("bad-id", b"hello").await;
        assert!(result.is_err());
        let err = result.expect_err("should be SessionNotFound");
        assert!(err.to_string().contains("bad-id"));
    }

    #[tokio::test]
    async fn test_poll_output_feeds_vte() {
        let (svc, backend, _) = make_service();
        let id = create_default_session(&svc).await;
        backend.inject_output(&id, b"Hello").await;

        let bytes = svc
            .poll_output(&id)
            .await
            .expect("poll_output should succeed");
        assert_eq!(bytes, 5);

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'H');
        assert_eq!(grid.cells[0][1].character, 'e');
        assert_eq!(grid.cells[0][2].character, 'l');
        assert_eq!(grid.cells[0][3].character, 'l');
        assert_eq!(grid.cells[0][4].character, 'o');
    }

    #[tokio::test]
    async fn test_poll_output_empty_returns_zero() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        let bytes = svc
            .poll_output(&id)
            .await
            .expect("poll_output should succeed");
        assert_eq!(bytes, 0);
    }

    #[tokio::test]
    async fn test_poll_output_publishes_event() {
        let (svc, backend, events) = make_service();
        let mut rx = events.subscribe();
        let id = create_default_session(&svc).await;
        backend.inject_output(&id, b"X").await;
        svc.poll_output(&id)
            .await
            .expect("poll_output should succeed");

        let event = rx.recv().await.expect("should receive event");
        match event {
            AppEvent::TerminalOutputReady { session_id } => {
                assert_eq!(session_id, id);
            }
            _ => panic!("Expected TerminalOutputReady event"),
        }
    }

    // -----------------------------------------------------------------------
    // VTE parser: plain text
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_plain_text() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"ABC")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        assert_eq!(grid.cells[0][1].character, 'B');
        assert_eq!(grid.cells[0][2].character, 'C');
        assert_eq!(grid.cursor_row, 0);
        assert_eq!(grid.cursor_col, 3);
    }

    #[tokio::test]
    async fn test_vte_plain_text_default_colors() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"X")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].fg_color, "#f9fafb");
        assert_eq!(grid.cells[0][0].bg_color, "#0a0a1a");
        assert!(!grid.cells[0][0].bold);
        assert!(!grid.cells[0][0].underline);
    }

    // -----------------------------------------------------------------------
    // VTE parser: newline, carriage return
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_newline() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // \r\n is the standard terminal newline (CR + LF)
        svc.feed_bytes(&id, b"A\r\nB")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        assert_eq!(grid.cells[1][0].character, 'B');
        assert_eq!(grid.cursor_row, 1);
        assert_eq!(grid.cursor_col, 1);
    }

    #[tokio::test]
    async fn test_vte_carriage_return() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"Hello\rWorld")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // "Hello" then CR resets column to 0, "World" overwrites
        assert_eq!(grid.cells[0][0].character, 'W');
        assert_eq!(grid.cells[0][1].character, 'o');
        assert_eq!(grid.cells[0][2].character, 'r');
        assert_eq!(grid.cells[0][3].character, 'l');
        assert_eq!(grid.cells[0][4].character, 'd');
    }

    #[tokio::test]
    async fn test_vte_cr_lf_sequence() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"Line1\r\nLine2")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'L');
        assert_eq!(grid.cells[0][4].character, '1');
        assert_eq!(grid.cells[1][0].character, 'L');
        assert_eq!(grid.cells[1][4].character, '2');
    }

    #[tokio::test]
    async fn test_vte_backspace() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Type "AB", backspace, then "C" — overwrites B with C
        svc.feed_bytes(&id, b"AB\x08C")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        assert_eq!(grid.cells[0][1].character, 'C');
    }

    #[tokio::test]
    async fn test_vte_tab() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"A\tB")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        // Tab from col 1 should go to col 8
        assert_eq!(grid.cells[0][8].character, 'B');
        assert_eq!(grid.cursor_col, 9);
    }

    // -----------------------------------------------------------------------
    // VTE parser: cursor movement (CSI A/B/C/D)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_cursor_up() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Move to row 3, then cursor up 2
        svc.feed_bytes(&id, b"\n\n\nX\x1b[2AY")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // X was at row 3, col 0 -> cursor moved to (3,1), then up 2 -> row 1
        assert_eq!(grid.cells[1][1].character, 'Y');
    }

    #[tokio::test]
    async fn test_vte_cursor_down() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"A\x1b[3BY")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        // Cursor was at (0,1), down 3 -> (3,1)
        assert_eq!(grid.cells[3][1].character, 'Y');
    }

    #[tokio::test]
    async fn test_vte_cursor_forward() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"A\x1b[5CZ")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        // Cursor was at (0,1), forward 5 -> (0,6)
        assert_eq!(grid.cells[0][6].character, 'Z');
    }

    #[tokio::test]
    async fn test_vte_cursor_back() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"ABCDE\x1b[3DX")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // "ABCDE" -> cursor at (0,5), back 3 -> (0,2), write X
        assert_eq!(grid.cells[0][0].character, 'A');
        assert_eq!(grid.cells[0][1].character, 'B');
        assert_eq!(grid.cells[0][2].character, 'X');
        assert_eq!(grid.cells[0][3].character, 'D');
        assert_eq!(grid.cells[0][4].character, 'E');
    }

    #[tokio::test]
    async fn test_vte_cursor_up_clamps_at_zero() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Start at row 0, try to go up 10 — should clamp at 0
        svc.feed_bytes(&id, b"\x1b[10AX")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cursor_row, 0);
        assert_eq!(grid.cells[0][0].character, 'X');
    }

    #[tokio::test]
    async fn test_vte_cursor_position_cup() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // CSI 5;10H — move to row 5, col 10 (1-indexed)
        svc.feed_bytes(&id, b"\x1b[5;10HZ")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[4][9].character, 'Z'); // 0-indexed
    }

    // -----------------------------------------------------------------------
    // VTE parser: SGR colors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_sgr_basic_foreground_colors() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;

        // Red foreground
        svc.feed_bytes(&id, b"\x1b[31mR")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'R');
        assert_eq!(grid.cells[0][0].fg_color, "#cc0000");
    }

    #[tokio::test]
    async fn test_vte_sgr_green_foreground() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"\x1b[32mG")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].fg_color, "#4e9a06");
    }

    #[tokio::test]
    async fn test_vte_sgr_bold() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"\x1b[1mB")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert!(grid.cells[0][0].bold);
    }

    #[tokio::test]
    async fn test_vte_sgr_underline() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"\x1b[4mU")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert!(grid.cells[0][0].underline);
    }

    #[tokio::test]
    async fn test_vte_sgr_reset() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Set bold+red, then reset, then print
        svc.feed_bytes(&id, b"\x1b[1;31mR\x1b[0mN")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].fg_color, "#cc0000");
        assert!(grid.cells[0][0].bold);
        // After reset
        assert_eq!(grid.cells[0][1].fg_color, DEFAULT_FG);
        assert!(!grid.cells[0][1].bold);
    }

    #[tokio::test]
    async fn test_vte_sgr_256_color_foreground() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // 256-color: ESC[38;5;9m — color index 9 = bright red
        svc.feed_bytes(&id, b"\x1b[38;5;9mX")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].fg_color, "#ef2929");
    }

    #[tokio::test]
    async fn test_vte_sgr_256_color_background() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // 256-color bg: ESC[48;5;4m — color index 4 = blue
        svc.feed_bytes(&id, b"\x1b[48;5;4mX")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].bg_color, "#3465a4");
    }

    #[tokio::test]
    async fn test_vte_sgr_default_foreground_reset() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Set red, then default fg (39)
        svc.feed_bytes(&id, b"\x1b[31mR\x1b[39mD")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].fg_color, "#cc0000");
        assert_eq!(grid.cells[0][1].fg_color, DEFAULT_FG);
    }

    #[tokio::test]
    async fn test_vte_sgr_background_color() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Green background (42)
        svc.feed_bytes(&id, b"\x1b[42mX")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].bg_color, "#4e9a06");
    }

    // -----------------------------------------------------------------------
    // VTE parser: erase line, erase display
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_erase_line_from_cursor() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Write "ABCDE", go back to col 2, erase to end of line
        svc.feed_bytes(&id, b"ABCDE\x1b[5D\x1b[2C\x1b[0K")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, 'A');
        assert_eq!(grid.cells[0][1].character, 'B');
        assert_eq!(grid.cells[0][2].character, ' '); // erased
        assert_eq!(grid.cells[0][3].character, ' '); // erased
        assert_eq!(grid.cells[0][4].character, ' '); // erased
    }

    #[tokio::test]
    async fn test_vte_erase_entire_line() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"ABCDE\x1b[2K")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // All cells on row 0 should be spaces
        for c in 0..5 {
            assert_eq!(grid.cells[0][c].character, ' ');
        }
    }

    #[tokio::test]
    async fn test_vte_erase_display() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"Line1\nLine2\x1b[2J")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // Entire display cleared
        assert_eq!(grid.cells[0][0].character, ' ');
        assert_eq!(grid.cells[1][0].character, ' ');
    }

    // -----------------------------------------------------------------------
    // VTE parser: line wrapping
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_line_wrapping() {
        let (svc, _, _) = make_service();
        // Create a small terminal (10 cols)
        let config = TerminalConfig {
            cols: 10,
            rows: 5,
            ..TerminalConfig::default()
        };
        let id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        // Write exactly 10 chars + 1 more to wrap
        svc.feed_bytes(&id, b"0123456789W")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // First row fully filled
        for i in 0..10 {
            let expected = std::char::from_digit(i as u32, 10)
                .expect("digit should convert");
            assert_eq!(grid.cells[0][i].character, expected);
        }
        // 'W' wrapped to row 1, col 0
        assert_eq!(grid.cells[1][0].character, 'W');
    }

    // -----------------------------------------------------------------------
    // Grid resize
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_resize_grid() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        svc.feed_bytes(&id, b"Hello")
            .await
            .expect("feed_bytes should succeed");

        svc.resize(&id, 40, 12)
            .await
            .expect("resize should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.rows, 12);
        assert_eq!(grid.cols, 40);
        assert_eq!(grid.cells.len(), 12);
        assert_eq!(grid.cells[0].len(), 40);
        // Content preserved
        assert_eq!(grid.cells[0][0].character, 'H');
    }

    #[tokio::test]
    async fn test_resize_clamps_cursor() {
        let (svc, _, _) = make_service();
        let config = TerminalConfig {
            cols: 80,
            rows: 24,
            ..TerminalConfig::default()
        };
        let id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        // Move cursor to row 20, col 50
        svc.feed_bytes(&id, b"\x1b[21;51H")
            .await
            .expect("feed_bytes should succeed");

        // Shrink to 5x5
        svc.resize(&id, 5, 5)
            .await
            .expect("resize should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert!(grid.cursor_row < 5, "cursor_row should be clamped");
        assert!(grid.cursor_col < 5, "cursor_col should be clamped");
    }

    // -----------------------------------------------------------------------
    // Multiple sessions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_multiple_sessions_independent() {
        let (svc, _, _) = make_service();
        let id1 = create_default_session(&svc).await;
        let id2 = create_default_session(&svc).await;

        svc.feed_bytes(&id1, b"AAA")
            .await
            .expect("feed_bytes should succeed");
        svc.feed_bytes(&id2, b"BBB")
            .await
            .expect("feed_bytes should succeed");

        let grid1 = svc.get_grid(&id1).await.expect("get_grid should succeed");
        let grid2 = svc.get_grid(&id2).await.expect("get_grid should succeed");

        assert_eq!(grid1.cells[0][0].character, 'A');
        assert_eq!(grid2.cells[0][0].character, 'B');
    }

    #[tokio::test]
    async fn test_multiple_sessions_listed() {
        let (svc, _, _) = make_service();
        let _id1 = create_default_session(&svc).await;
        let _id2 = create_default_session(&svc).await;
        let _id3 = create_default_session(&svc).await;

        let sessions = svc.list_sessions().await;
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_close_one_of_multiple() {
        let (svc, _, _) = make_service();
        let id1 = create_default_session(&svc).await;
        let id2 = create_default_session(&svc).await;

        svc.close_session(&id1)
            .await
            .expect("close_session should succeed");

        let sessions = svc.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, id2);
    }

    // -----------------------------------------------------------------------
    // Session not found errors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_grid_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.get_grid("does-not-exist").await;
        assert!(result.is_err());
        let err = result.expect_err("should be SessionNotFound");
        assert!(err.to_string().contains("does-not-exist"));
    }

    #[tokio::test]
    async fn test_feed_bytes_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.feed_bytes("nope", b"data").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resize_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.resize("nope", 80, 24).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_set_active_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.set_active("nope", true).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_set_title_nonexistent_session() {
        let (svc, _, _) = make_service();
        let result = svc.set_title("nope", "title").await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Session metadata
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_set_active_flag() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;

        svc.set_active(&id, false)
            .await
            .expect("set_active should succeed");

        let sessions = svc.list_sessions().await;
        assert!(!sessions[0].is_active);

        svc.set_active(&id, true)
            .await
            .expect("set_active should succeed");

        let sessions = svc.list_sessions().await;
        assert!(sessions[0].is_active);
    }

    #[tokio::test]
    async fn test_set_title() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;

        svc.set_title(&id, "My Custom Title")
            .await
            .expect("set_title should succeed");

        let sessions = svc.list_sessions().await;
        assert_eq!(sessions[0].title, "My Custom Title");
    }

    #[tokio::test]
    async fn test_session_info_shell_name() {
        let (svc, _, _) = make_service();
        let config = TerminalConfig {
            shell: Some("/usr/bin/zsh".to_string()),
            ..TerminalConfig::default()
        };
        let _id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        let sessions = svc.list_sessions().await;
        assert_eq!(sessions[0].shell, "/usr/bin/zsh");
        assert!(sessions[0].title.contains("zsh"));
    }

    // -----------------------------------------------------------------------
    // VTE parser: scroll on bottom
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_scroll_at_bottom() {
        let (svc, _, _) = make_service();
        let config = TerminalConfig {
            cols: 10,
            rows: 3,
            ..TerminalConfig::default()
        };
        let id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        // Fill 3 lines then add a 4th — should scroll
        // Use \r\n for proper terminal newlines (CR+LF)
        svc.feed_bytes(&id, b"A\r\nB\r\nC\r\nD")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        // After scroll, line A is gone
        assert_eq!(grid.cells[0][0].character, 'B');
        assert_eq!(grid.cells[1][0].character, 'C');
        assert_eq!(grid.cells[2][0].character, 'D');
    }

    // -----------------------------------------------------------------------
    // 256-color conversion
    // -----------------------------------------------------------------------

    #[test]
    fn test_color_256_standard() {
        assert_eq!(color_256_to_hex(0), "#000000");
        assert_eq!(color_256_to_hex(1), "#cc0000");
        assert_eq!(color_256_to_hex(7), "#d3d7cf");
    }

    #[test]
    fn test_color_256_bright() {
        assert_eq!(color_256_to_hex(8), "#555753");
        assert_eq!(color_256_to_hex(15), "#eeeeec");
    }

    #[test]
    fn test_color_256_cube() {
        // Index 16 = 0,0,0 in the cube = (0,0,0)
        assert_eq!(color_256_to_hex(16), "#000000");
        // Index 196 = 5,0,0 in the cube = (255,0,0)
        assert_eq!(color_256_to_hex(196), "#ff0000");
    }

    #[test]
    fn test_color_256_grayscale() {
        // Index 232 = grayscale level 8
        assert_eq!(color_256_to_hex(232), "#080808");
        // Index 255 = 8 + 10 * 23 = 238 = 0xee
        assert_eq!(color_256_to_hex(255), "#eeeeee");
    }

    #[test]
    fn test_color_256_out_of_range() {
        // Out of range returns default foreground
        assert_eq!(color_256_to_hex(256), DEFAULT_FG);
        assert_eq!(color_256_to_hex(999), DEFAULT_FG);
    }

    // -----------------------------------------------------------------------
    // Shell name extraction
    // -----------------------------------------------------------------------

    #[test]
    fn test_shell_name_full_path() {
        assert_eq!(shell_name("/bin/bash"), "bash");
        assert_eq!(shell_name("/usr/bin/zsh"), "zsh");
        assert_eq!(shell_name("/bin/sh"), "sh");
    }

    #[test]
    fn test_shell_name_bare() {
        assert_eq!(shell_name("bash"), "bash");
    }

    // -----------------------------------------------------------------------
    // Grid initial state
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initial_grid_all_spaces() {
        let (svc, _, _) = make_service();
        let config = TerminalConfig {
            cols: 5,
            rows: 3,
            ..TerminalConfig::default()
        };
        let id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.rows, 3);
        assert_eq!(grid.cols, 5);
        assert!(grid.cursor_visible);
        for row in &grid.cells {
            for cell in row {
                assert_eq!(cell.character, ' ');
            }
        }
    }

    #[tokio::test]
    async fn test_initial_cursor_at_origin() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cursor_row, 0);
        assert_eq!(grid.cursor_col, 0);
    }

    // -----------------------------------------------------------------------
    // VTE parser: erase from start of line
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_vte_erase_line_from_start() {
        let (svc, _, _) = make_service();
        let id = create_default_session(&svc).await;
        // Write "ABCDE", cursor at col 5, move back to col 2, erase from start
        svc.feed_bytes(&id, b"ABCDE\x1b[5D\x1b[2C\x1b[1K")
            .await
            .expect("feed_bytes should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.cells[0][0].character, ' '); // erased
        assert_eq!(grid.cells[0][1].character, ' '); // erased
        assert_eq!(grid.cells[0][2].character, ' '); // erased (inclusive)
        assert_eq!(grid.cells[0][3].character, 'D'); // preserved
        assert_eq!(grid.cells[0][4].character, 'E'); // preserved
    }

    // -----------------------------------------------------------------------
    // Custom config
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_custom_config_dimensions() {
        let (svc, _, _) = make_service();
        let config = TerminalConfig {
            cols: 132,
            rows: 43,
            ..TerminalConfig::default()
        };
        let id = svc
            .create_session(&config)
            .await
            .expect("create_session should succeed");

        let grid = svc.get_grid(&id).await.expect("get_grid should succeed");
        assert_eq!(grid.rows, 43);
        assert_eq!(grid.cols, 132);
        assert_eq!(grid.cells.len(), 43);
        assert_eq!(grid.cells[0].len(), 132);
    }
}
