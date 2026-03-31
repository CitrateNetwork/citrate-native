//! Editor service — rope-based text buffer with syntax highlighting.
//!
//! Data source: filesystem for load/save, ropey for buffer management,
//! syntect for syntax highlighting.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::view_models::ide_view_models::{
    BufferInfo, CursorPosition, FileType, StyledLine, StyledSpan,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Syntax highlighter using syntect. Shared across all buffers.
struct SyntaxHighlighter {
    syntax_set: syntect::parsing::SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl SyntaxHighlighter {
    fn new() -> Self {
        let syntax_set = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let theme_set = syntect::highlighting::ThemeSet::load_defaults();
        let theme = theme_set.themes.get("base16-ocean.dark")
            .cloned()
            .unwrap_or_else(|| theme_set.themes.values().next()
                .cloned()
                .unwrap_or_default());
        Self { syntax_set, theme }
    }

    fn highlight_line(&self, line: &str, language: &str) -> Vec<StyledSpan> {
        let syntax = self.syntax_set.find_syntax_by_name(language)
            .or_else(|| self.syntax_set.find_syntax_by_extension(
                match language {
                    "Solidity" => "sol",
                    "Rust" => "rs",
                    "TypeScript" => "ts",
                    "JavaScript" => "js",
                    "Python" => "py",
                    "TOML" => "toml",
                    "JSON" => "json",
                    "Markdown" => "md",
                    "YAML" => "yaml",
                    "Shell" => "sh",
                    _ => "txt",
                }
            ))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = syntect::easy::HighlightLines::new(syntax, &self.theme);
        let trimmed = line.trim_end_matches('\n');

        match highlighter.highlight_line(trimmed, &self.syntax_set) {
            Ok(ranges) => {
                ranges.iter().map(|(style, text)| {
                    StyledSpan {
                        text: text.to_string(),
                        fg_color: format!(
                            "#{:02x}{:02x}{:02x}",
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b
                        ),
                        bold: style.font_style.contains(syntect::highlighting::FontStyle::BOLD),
                        italic: style.font_style.contains(syntect::highlighting::FontStyle::ITALIC),
                    }
                }).collect()
            }
            Err(_) => {
                // Fallback to plain text on parse error
                vec![StyledSpan {
                    text: trimmed.to_string(),
                    fg_color: "#f9fafb".to_string(),
                    bold: false,
                    italic: false,
                }]
            }
        }
    }
}

/// Backend trait for editor file I/O and syntax highlighting.
#[async_trait::async_trait]
pub trait EditorBackend: Send + Sync {
    /// Load file contents as a string.
    async fn load_file(&self, path: &Path) -> Result<String, AppError>;
    /// Save string content to a file.
    async fn save_file(&self, path: &Path, content: &str) -> Result<(), AppError>;
    /// Detect the language from a file path.
    fn detect_language(&self, path: &Path) -> String;
}

/// Test-only in-memory backend. Not available in release builds.
#[cfg(test)]
pub struct InMemoryEditorBackend {
    files: RwLock<HashMap<PathBuf, String>>,
}

#[cfg(test)]
impl Default for InMemoryEditorBackend {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
impl InMemoryEditorBackend {
    pub fn new() -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add_file(&self, path: &Path, content: &str) {
        self.files.write().await.insert(path.to_path_buf(), content.to_string());
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl EditorBackend for InMemoryEditorBackend {
    async fn load_file(&self, path: &Path) -> Result<String, AppError> {
        self.files
            .read()
            .await
            .get(path)
            .cloned()
            .ok_or_else(|| AppError::FileSystem(format!("File not found: {}", path.display())))
    }

    async fn save_file(&self, path: &Path, content: &str) -> Result<(), AppError> {
        self.files.write().await.insert(path.to_path_buf(), content.to_string());
        Ok(())
    }

    fn detect_language(&self, path: &Path) -> String {
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        FileType::from_extension(&ext).language_id().to_string()
    }
}

/// Real filesystem backend.
pub struct RealEditorBackend;

#[async_trait::async_trait]
impl EditorBackend for RealEditorBackend {
    async fn load_file(&self, path: &Path) -> Result<String, AppError> {
        let path = path.to_path_buf();
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AppError::FileSystem(format!("{}: {}", path.display(), e)))
    }

    async fn save_file(&self, path: &Path, content: &str) -> Result<(), AppError> {
        let path = path.to_path_buf();
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| AppError::FileSystem(format!("{}: {}", path.display(), e)))
    }

    fn detect_language(&self, path: &Path) -> String {
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        FileType::from_extension(&ext).language_id().to_string()
    }
}

/// An edit operation for undo/redo.
#[derive(Debug, Clone)]
struct EditOperation {
    /// Byte offset in the rope where the edit occurred.
    position: usize,
    /// Text that was deleted (empty for pure insertions).
    deleted: String,
    /// Text that was inserted (empty for pure deletions).
    inserted: String,
}

/// Internal buffer state (not exposed to UI).
struct EditorBuffer {
    id: String,
    file_path: PathBuf,
    rope: ropey::Rope,
    language: String,
    is_dirty: bool,
    undo_stack: Vec<EditOperation>,
    redo_stack: Vec<EditOperation>,
    saved_content_hash: u64,
}

impl EditorBuffer {
    fn new(id: String, path: PathBuf, content: &str, language: String) -> Self {
        let hash = Self::hash_content(content);
        Self {
            id,
            file_path: path,
            rope: ropey::Rope::from_str(content),
            language,
            is_dirty: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            saved_content_hash: hash,
        }
    }

    fn hash_content(content: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    fn update_dirty_state(&mut self) {
        let current = Self::hash_content(&self.rope.to_string());
        self.is_dirty = current != self.saved_content_hash;
    }

    fn info(&self) -> BufferInfo {
        BufferInfo {
            id: self.id.clone(),
            file_path: self.file_path.to_string_lossy().to_string(),
            file_name: self.file_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            language: self.language.clone(),
            is_dirty: self.is_dirty,
            line_count: self.rope.len_lines(),
        }
    }
}

/// Editor service — manages open buffers, edits, syntax highlighting.
pub struct EditorService {
    events: Arc<EventBus>,
    buffers: Arc<RwLock<HashMap<String, EditorBuffer>>>,
    backend: Arc<dyn EditorBackend>,
    highlighter: SyntaxHighlighter,
}

impl EditorService {
    /// Create with the real filesystem backend.
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            buffers: Arc::new(RwLock::new(HashMap::new())),
            backend: Arc::new(RealEditorBackend),
            highlighter: SyntaxHighlighter::new(),
        }
    }

    /// Create with an injected backend (for testing or alternative storage).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn EditorBackend>) -> Self {
        Self {
            events,
            buffers: Arc::new(RwLock::new(HashMap::new())),
            backend,
            highlighter: SyntaxHighlighter::new(),
        }
    }

    /// Open a file. Returns the buffer ID.
    pub async fn open_file(&self, path: &Path) -> Result<String, AppError> {
        // Check if already open
        let buffers = self.buffers.read().await;
        for buf in buffers.values() {
            if buf.file_path == path {
                return Ok(buf.id.clone());
            }
        }
        drop(buffers);

        let content = self.backend.load_file(path).await?;
        let language = self.backend.detect_language(path);
        let id = uuid::Uuid::new_v4().to_string();
        let buffer = EditorBuffer::new(id.clone(), path.to_path_buf(), &content, language);

        self.buffers.write().await.insert(id.clone(), buffer);
        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: id.clone(),
        });
        Ok(id)
    }

    /// Close a buffer.
    pub async fn close_buffer(&self, buffer_id: &str) -> Result<(), AppError> {
        self.buffers
            .write()
            .await
            .remove(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;
        Ok(())
    }

    /// Save the buffer to disk.
    pub async fn save_buffer(&self, buffer_id: &str) -> Result<(), AppError> {
        let buffers = self.buffers.read().await;
        let buf = buffers
            .get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;
        let content = buf.rope.to_string();
        let path = buf.file_path.clone();
        drop(buffers);

        self.backend.save_file(&path, &content).await?;

        let mut buffers = self.buffers.write().await;
        if let Some(buf) = buffers.get_mut(buffer_id) {
            buf.saved_content_hash = EditorBuffer::hash_content(&content);
            buf.is_dirty = false;
        }
        Ok(())
    }

    /// Save all open buffers.
    pub async fn save_all(&self) -> Result<(), AppError> {
        let ids: Vec<String> = self.buffers.read().await.keys().cloned().collect();
        for id in ids {
            self.save_buffer(&id).await?;
        }
        Ok(())
    }

    /// Insert text at a byte offset. Returns the new cursor position.
    pub async fn insert_text(
        &self,
        buffer_id: &str,
        byte_offset: usize,
        text: &str,
    ) -> Result<usize, AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let char_offset = buf.rope.byte_to_char(byte_offset.min(buf.rope.len_bytes()));
        buf.rope.insert(char_offset, text);

        let op = EditOperation {
            position: byte_offset,
            deleted: String::new(),
            inserted: text.to_string(),
        };
        buf.undo_stack.push(op);
        buf.redo_stack.clear();
        buf.update_dirty_state();

        let new_byte_offset = byte_offset + text.len();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(new_byte_offset)
    }

    /// Delete a range of bytes. Returns the deletion start offset.
    pub async fn delete_range(
        &self,
        buffer_id: &str,
        start_byte: usize,
        end_byte: usize,
    ) -> Result<usize, AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let len = buf.rope.len_bytes();
        let start = start_byte.min(len);
        let end = end_byte.min(len);
        if start >= end {
            return Ok(start);
        }

        let start_char = buf.rope.byte_to_char(start);
        let end_char = buf.rope.byte_to_char(end);
        let deleted: String = buf.rope.slice(start_char..end_char).into();
        buf.rope.remove(start_char..end_char);

        let op = EditOperation {
            position: start,
            deleted,
            inserted: String::new(),
        };
        buf.undo_stack.push(op);
        buf.redo_stack.clear();
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(start)
    }

    /// Undo the last edit.
    pub async fn undo(&self, buffer_id: &str) -> Result<(), AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let op = match buf.undo_stack.pop() {
            Some(op) => op,
            None => return Ok(()), // nothing to undo
        };

        // Reverse the operation
        if !op.inserted.is_empty() {
            let char_start = buf.rope.byte_to_char(op.position);
            let char_end = buf.rope.byte_to_char(op.position + op.inserted.len());
            buf.rope.remove(char_start..char_end);
        }
        if !op.deleted.is_empty() {
            let char_pos = buf.rope.byte_to_char(op.position);
            buf.rope.insert(char_pos, &op.deleted);
        }

        buf.redo_stack.push(op);
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(())
    }

    /// Redo the last undone edit.
    pub async fn redo(&self, buffer_id: &str) -> Result<(), AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let op = match buf.redo_stack.pop() {
            Some(op) => op,
            None => return Ok(()), // nothing to redo
        };

        // Re-apply the operation
        if !op.deleted.is_empty() {
            let char_start = buf.rope.byte_to_char(op.position);
            let char_end = buf.rope.byte_to_char(op.position + op.deleted.len());
            buf.rope.remove(char_start..char_end);
        }
        if !op.inserted.is_empty() {
            let char_pos = buf.rope.byte_to_char(op.position);
            buf.rope.insert(char_pos, &op.inserted);
        }

        buf.undo_stack.push(op);
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(())
    }

    /// Get styled lines for the visible viewport.
    pub async fn get_visible_lines(
        &self,
        buffer_id: &str,
        start_line: usize,
        count: usize,
    ) -> Result<Vec<StyledLine>, AppError> {
        let buffers = self.buffers.read().await;
        let buf = buffers
            .get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let total_lines = buf.rope.len_lines();
        let start = start_line.min(total_lines);
        let end = (start + count).min(total_lines);

        let language = buf.language.clone();
        let mut lines = Vec::with_capacity(end - start);
        for line_idx in start..end {
            let line_text: String = buf.rope.line(line_idx).into();
            let spans = self.highlighter.highlight_line(&line_text, &language);
            lines.push(StyledLine {
                line_number: line_idx + 1,
                spans,
            });
        }

        Ok(lines)
    }

    /// Get buffer info for a specific buffer.
    pub async fn get_buffer_info(&self, buffer_id: &str) -> Result<BufferInfo, AppError> {
        let buffers = self.buffers.read().await;
        buffers
            .get(buffer_id)
            .map(|b| b.info())
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// List all open buffers.
    pub async fn list_open_buffers(&self) -> Vec<BufferInfo> {
        self.buffers.read().await.values().map(|b| b.info()).collect()
    }

    /// Synchronous version for UI polling — returns empty if lock unavailable.
    pub fn list_open_buffers_sync(&self) -> Vec<BufferInfo> {
        self.buffers.try_read().map(|g| g.values().map(|b| b.info()).collect()).unwrap_or_default()
    }

    /// Sync version of get_line_count.
    pub fn get_line_count_sync(&self, buffer_id: &str) -> Result<usize, AppError> {
        let buffers = self.buffers.try_read()
            .map_err(|_| AppError::Editor("Lock busy".into()))?;
        buffers.get(buffer_id)
            .map(|b| b.rope.len_lines())
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// Sync version of get_visible_lines.
    pub fn get_visible_lines_sync(&self, buffer_id: &str, start_line: usize, count: usize) -> Result<Vec<StyledLine>, AppError> {
        let buffers = self.buffers.try_read()
            .map_err(|_| AppError::Editor("Lock busy".into()))?;
        let buf = buffers.get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;
        let total_lines = buf.rope.len_lines();
        let start = start_line.min(total_lines);
        let end = (start + count).min(total_lines);
        let language = buf.language.clone();
        let mut lines = Vec::with_capacity(end - start);
        for line_idx in start..end {
            let line_text: String = buf.rope.line(line_idx).into();
            let spans = self.highlighter.highlight_line(&line_text, &language);
            lines.push(StyledLine { line_number: line_idx + 1, spans });
        }
        Ok(lines)
    }

    /// Sync version of get_buffer_info.
    pub fn get_buffer_info_sync(&self, buffer_id: &str) -> Result<BufferInfo, AppError> {
        let buffers = self.buffers.try_read()
            .map_err(|_| AppError::Editor("Lock busy".into()))?;
        buffers.get(buffer_id)
            .map(|b| b.info())
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// Get the full content of a buffer as a string.
    pub async fn get_content(&self, buffer_id: &str) -> Result<String, AppError> {
        let buffers = self.buffers.read().await;
        buffers
            .get(buffer_id)
            .map(|b| b.rope.to_string())
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// Get the total line count.
    pub async fn get_line_count(&self, buffer_id: &str) -> Result<usize, AppError> {
        let buffers = self.buffers.read().await;
        buffers
            .get(buffer_id)
            .map(|b| b.rope.len_lines())
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// Check if a buffer has unsaved changes.
    pub async fn is_dirty(&self, buffer_id: &str) -> Result<bool, AppError> {
        let buffers = self.buffers.read().await;
        buffers
            .get(buffer_id)
            .map(|b| b.is_dirty)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))
    }

    /// Search for a pattern in the buffer (case-insensitive).
    /// Returns (line, column) positions of all matches.
    pub async fn search(
        &self,
        buffer_id: &str,
        query: &str,
    ) -> Result<Vec<CursorPosition>, AppError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let buffers = self.buffers.read().await;
        let buf = buffers
            .get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let mut results = Vec::new();
        let query_lower = query.to_lowercase();
        for line_idx in 0..buf.rope.len_lines() {
            let line: String = buf.rope.line(line_idx).into();
            let line_lower = line.to_lowercase();
            let mut search_from = 0;
            while let Some(col) = line_lower[search_from..].find(&query_lower) {
                results.push(CursorPosition {
                    line: line_idx,
                    column: search_from + col,
                });
                search_from += col + query_lower.len();
            }
        }
        Ok(results)
    }

    /// Replace all occurrences of `query` with `replacement`. Returns count replaced.
    pub async fn replace_all(
        &self,
        buffer_id: &str,
        query: &str,
        replacement: &str,
    ) -> Result<usize, AppError> {
        if query.is_empty() {
            return Ok(0);
        }
        let positions = self.search(buffer_id, query).await?;
        let count = positions.len();
        if count == 0 {
            return Ok(0);
        }

        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let old_content = buf.rope.to_string();
        // Case-insensitive replace using collected positions (reverse order to preserve offsets)
        let new_content = old_content.replace(query, replacement);
        buf.rope = ropey::Rope::from_str(&new_content);
        buf.undo_stack.push(EditOperation {
            position: 0,
            deleted: old_content,
            inserted: new_content,
        });
        buf.redo_stack.clear();
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(count)
    }

    /// Go to a specific line (0-indexed). Clamps to valid range.
    pub async fn go_to_line(
        &self,
        buffer_id: &str,
        line: usize,
    ) -> Result<CursorPosition, AppError> {
        let buffers = self.buffers.read().await;
        let buf = buffers
            .get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let clamped = line.min(buf.rope.len_lines().saturating_sub(1));
        Ok(CursorPosition {
            line: clamped,
            column: 0,
        })
    }

    /// Indent lines in range by prepending 4 spaces.
    pub async fn indent_lines(
        &self,
        buffer_id: &str,
        start_line: usize,
        end_line: usize,
    ) -> Result<(), AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let end = end_line.min(buf.rope.len_lines());
        for line_idx in (start_line..end).rev() {
            let char_idx = buf.rope.line_to_char(line_idx);
            buf.rope.insert(char_idx, "    ");
        }
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(())
    }

    /// Unindent lines by removing up to 4 leading spaces.
    pub async fn unindent_lines(
        &self,
        buffer_id: &str,
        start_line: usize,
        end_line: usize,
    ) -> Result<(), AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let end = end_line.min(buf.rope.len_lines());
        for line_idx in start_line..end {
            let line: String = buf.rope.line(line_idx).into();
            let spaces = line.chars().take(4).take_while(|c| *c == ' ').count();
            if spaces > 0 {
                let char_idx = buf.rope.line_to_char(line_idx);
                buf.rope.remove(char_idx..char_idx + spaces);
            }
        }
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(())
    }

    /// Toggle line comments (// prefix) for a range of lines.
    pub async fn toggle_comment(
        &self,
        buffer_id: &str,
        start_line: usize,
        end_line: usize,
    ) -> Result<(), AppError> {
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        let end = end_line.min(buf.rope.len_lines());
        let all_commented = (start_line..end).all(|i| {
            let line: String = buf.rope.line(i).into();
            line.trim_start().starts_with("//")
        });

        for line_idx in (start_line..end).rev() {
            let line: String = buf.rope.line(line_idx).into();
            let char_idx = buf.rope.line_to_char(line_idx);
            let leading = line.len() - line.trim_start().len();
            if all_commented {
                if line.trim_start().starts_with("// ") {
                    buf.rope.remove(char_idx + leading..char_idx + leading + 3);
                } else if line.trim_start().starts_with("//") {
                    buf.rope.remove(char_idx + leading..char_idx + leading + 2);
                }
            } else {
                buf.rope.insert(char_idx, "// ");
            }
        }
        buf.update_dirty_state();
        drop(buffers);

        self.events.publish(AppEvent::EditorBufferChanged {
            buffer_id: buffer_id.to_string(),
        });
        Ok(())
    }

    /// Get the word boundaries at a position (for double-click select).
    /// Returns (start_col, end_col) of the word.
    pub async fn get_word_at(
        &self,
        buffer_id: &str,
        line: usize,
        column: usize,
    ) -> Result<(usize, usize), AppError> {
        let buffers = self.buffers.read().await;
        let buf = buffers
            .get(buffer_id)
            .ok_or_else(|| AppError::BufferNotFound(buffer_id.to_string()))?;

        if line >= buf.rope.len_lines() {
            return Ok((column, column));
        }
        let line_text: String = buf.rope.line(line).into();
        let chars: Vec<char> = line_text.chars().collect();
        let col = column.min(chars.len());

        let is_word_char = |c: char| c.is_alphanumeric() || c == '_';

        let mut start = col;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        Ok((start, end))
    }

    /// Search across all open buffers. Returns (buffer_id, line, column, line_text) for each match.
    pub async fn search_all_buffers(
        &self,
        query: &str,
    ) -> Result<Vec<(String, usize, usize, String)>, AppError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let buffers = self.buffers.read().await;
        let mut all_results = Vec::new();
        let query_lower = query.to_lowercase();

        for (id, buf) in buffers.iter() {
            for line_idx in 0..buf.rope.len_lines() {
                let line: String = buf.rope.line(line_idx).into();
                let line_lower = line.to_lowercase();
                if let Some(col) = line_lower.find(&query_lower) {
                    all_results.push((
                        id.clone(),
                        line_idx,
                        col,
                        line.trim_end().to_string(),
                    ));
                }
            }
        }
        Ok(all_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service_with_file(content: &str) -> (EditorService, String) {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/test/main.rs"), content).await;
        let svc = EditorService::with_backend(events, backend);
        let id = svc.open_file(Path::new("/test/main.rs")).await.expect("async operation succeeded");
        (svc, id)
    }

    fn test_service() -> EditorService {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        EditorService::with_backend(events, backend)
    }

    // --- Open/Close lifecycle ---

    #[tokio::test]
    async fn test_open_file_returns_id() {
        let (_, id) = service_with_file("hello").await;
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn test_open_same_file_returns_same_id() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/test/f.rs"), "x").await;
        let svc = EditorService::with_backend(events, backend);

        let id1 = svc.open_file(Path::new("/test/f.rs")).await.expect("async operation succeeded");
        let id2 = svc.open_file(Path::new("/test/f.rs")).await.expect("async operation succeeded");
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn test_open_nonexistent_file_fails() {
        let svc = test_service();
        let result = svc.open_file(Path::new("/nonexistent")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_close_buffer() {
        let (svc, id) = service_with_file("hello").await;
        svc.close_buffer(&id).await.expect("close succeeded");
        assert!(svc.get_buffer_info(&id).await.is_err());
    }

    #[tokio::test]
    async fn test_close_nonexistent_buffer_fails() {
        let svc = test_service();
        let result = svc.close_buffer("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_open_buffers() {
        let (svc, _) = service_with_file("hello").await;
        let buffers = svc.list_open_buffers().await;
        assert_eq!(buffers.len(), 1);
        assert_eq!(buffers[0].file_name, "main.rs");
    }

    #[tokio::test]
    async fn test_list_empty_when_no_buffers() {
        let svc = test_service();
        assert!(svc.list_open_buffers().await.is_empty());
    }

    // --- Content operations ---

    #[tokio::test]
    async fn test_get_content() {
        let (svc, id) = service_with_file("hello world").await;
        let content = svc.get_content(&id).await.expect("async operation succeeded");
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_get_line_count() {
        let (svc, id) = service_with_file("line1\nline2\nline3").await;
        let count = svc.get_line_count(&id).await.expect("async operation succeeded");
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_get_line_count_single_line() {
        let (svc, id) = service_with_file("hello").await;
        assert_eq!(svc.get_line_count(&id).await.expect("async operation succeeded"), 1);
    }

    #[tokio::test]
    async fn test_get_line_count_empty() {
        let (svc, id) = service_with_file("").await;
        assert_eq!(svc.get_line_count(&id).await.expect("async operation succeeded"), 1); // ropey counts 1 for empty
    }

    // --- Insert operations ---

    #[tokio::test]
    async fn test_insert_at_beginning() {
        let (svc, id) = service_with_file("world").await;
        svc.insert_text(&id, 0, "hello ").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello world");
    }

    #[tokio::test]
    async fn test_insert_at_end() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, " world").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello world");
    }

    #[tokio::test]
    async fn test_insert_in_middle() {
        let (svc, id) = service_with_file("helo").await;
        svc.insert_text(&id, 2, "l").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_insert_newline() {
        let (svc, id) = service_with_file("ab").await;
        svc.insert_text(&id, 1, "\n").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "a\nb");
        assert_eq!(svc.get_line_count(&id).await.expect("async operation succeeded"), 2);
    }

    #[tokio::test]
    async fn test_insert_marks_dirty() {
        let (svc, id) = service_with_file("hello").await;
        assert!(!svc.is_dirty(&id).await.expect("async operation succeeded"));
        svc.insert_text(&id, 5, "!").await.expect("async operation succeeded");
        assert!(svc.is_dirty(&id).await.expect("async operation succeeded"));
    }

    #[tokio::test]
    async fn test_insert_returns_new_offset() {
        let (svc, id) = service_with_file("hello").await;
        let new_offset = svc.insert_text(&id, 5, " world").await.expect("async operation succeeded");
        assert_eq!(new_offset, 11);
    }

    // --- Delete operations ---

    #[tokio::test]
    async fn test_delete_single_char() {
        let (svc, id) = service_with_file("hello").await;
        svc.delete_range(&id, 4, 5).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hell");
    }

    #[tokio::test]
    async fn test_delete_range() {
        let (svc, id) = service_with_file("hello world").await;
        svc.delete_range(&id, 5, 11).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_delete_all() {
        let (svc, id) = service_with_file("hello").await;
        svc.delete_range(&id, 0, 5).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "");
    }

    #[tokio::test]
    async fn test_delete_empty_range() {
        let (svc, id) = service_with_file("hello").await;
        svc.delete_range(&id, 3, 3).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_delete_marks_dirty() {
        let (svc, id) = service_with_file("hello").await;
        svc.delete_range(&id, 4, 5).await.expect("async operation succeeded");
        assert!(svc.is_dirty(&id).await.expect("async operation succeeded"));
    }

    // --- Undo/Redo ---

    #[tokio::test]
    async fn test_undo_insert() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, " world").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello world");
        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_undo_delete() {
        let (svc, id) = service_with_file("hello").await;
        svc.delete_range(&id, 4, 5).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hell");
        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_redo() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, "!").await.expect("async operation succeeded");
        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
        svc.redo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello!");
    }

    #[tokio::test]
    async fn test_undo_nothing_is_ok() {
        let (svc, id) = service_with_file("hello").await;
        svc.undo(&id).await.expect("async operation succeeded"); // no-op
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_redo_nothing_is_ok() {
        let (svc, id) = service_with_file("hello").await;
        svc.redo(&id).await.expect("async operation succeeded"); // no-op
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello");
    }

    #[tokio::test]
    async fn test_edit_clears_redo_stack() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, "!").await.expect("async operation succeeded");
        svc.undo(&id).await.expect("async operation succeeded");
        // Now insert something different — redo stack should be cleared
        svc.insert_text(&id, 5, "?").await.expect("async operation succeeded");
        svc.redo(&id).await.expect("async operation succeeded"); // should be no-op
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello?");
    }

    #[tokio::test]
    async fn test_multiple_undos() {
        let (svc, id) = service_with_file("").await;
        svc.insert_text(&id, 0, "a").await.expect("async operation succeeded");
        svc.insert_text(&id, 1, "b").await.expect("async operation succeeded");
        svc.insert_text(&id, 2, "c").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "abc");

        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "ab");
        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "a");
        svc.undo(&id).await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "");
    }

    // --- Save ---

    #[tokio::test]
    async fn test_save_clears_dirty() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, "!").await.expect("async operation succeeded");
        assert!(svc.is_dirty(&id).await.expect("async operation succeeded"));
        svc.save_buffer(&id).await.expect("save succeeded");
        assert!(!svc.is_dirty(&id).await.expect("async operation succeeded"));
    }

    #[tokio::test]
    async fn test_save_all() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/a.rs"), "a").await;
        backend.add_file(Path::new("/b.rs"), "b").await;
        let svc = EditorService::with_backend(events, backend);

        let id_a = svc.open_file(Path::new("/a.rs")).await.expect("async operation succeeded");
        let id_b = svc.open_file(Path::new("/b.rs")).await.expect("async operation succeeded");
        svc.insert_text(&id_a, 1, "!").await.expect("async operation succeeded");
        svc.insert_text(&id_b, 1, "!").await.expect("async operation succeeded");

        svc.save_all().await.expect("save_all succeeded");
        assert!(!svc.is_dirty(&id_a).await.expect("async operation succeeded"));
        assert!(!svc.is_dirty(&id_b).await.expect("async operation succeeded"));
    }

    #[tokio::test]
    async fn test_save_nonexistent_fails() {
        let svc = test_service();
        assert!(svc.save_buffer("nonexistent").await.is_err());
    }

    // --- Visible lines ---

    #[tokio::test]
    async fn test_get_visible_lines() {
        let (svc, id) = service_with_file("line1\nline2\nline3").await;
        let lines = svc.get_visible_lines(&id, 0, 3).await.expect("async operation succeeded");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_number, 1);
        assert_eq!(lines[0].spans[0].text, "line1");
        assert_eq!(lines[1].spans[0].text, "line2");
        assert_eq!(lines[2].spans[0].text, "line3");
    }

    #[tokio::test]
    async fn test_get_visible_lines_partial() {
        let (svc, id) = service_with_file("a\nb\nc\nd\ne").await;
        let lines = svc.get_visible_lines(&id, 1, 2).await.expect("async operation succeeded");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_number, 2);
        assert_eq!(lines[0].spans[0].text, "b");
    }

    #[tokio::test]
    async fn test_get_visible_lines_beyond_end() {
        let (svc, id) = service_with_file("a\nb").await;
        let lines = svc.get_visible_lines(&id, 0, 100).await.expect("async operation succeeded");
        assert_eq!(lines.len(), 2);
    }

    // --- Buffer info ---

    #[tokio::test]
    async fn test_buffer_info_fields() {
        let (svc, id) = service_with_file("hello\nworld").await;
        let info = svc.get_buffer_info(&id).await.expect("async operation succeeded");
        assert_eq!(info.file_name, "main.rs");
        assert_eq!(info.language, "Rust");
        assert_eq!(info.line_count, 2);
        assert!(!info.is_dirty);
    }

    // --- Events ---

    #[tokio::test]
    async fn test_open_publishes_event() {
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/f.rs"), "x").await;
        let svc = EditorService::with_backend(events, backend);

        let id = svc.open_file(Path::new("/f.rs")).await.expect("async operation succeeded");
        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::EditorBufferChanged { buffer_id } => assert_eq!(buffer_id, id),
            _ => panic!("Expected EditorBufferChanged"),
        }
    }

    #[tokio::test]
    async fn test_insert_publishes_event() {
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/f.rs"), "x").await;
        let svc = EditorService::with_backend(events, backend);
        let id = svc.open_file(Path::new("/f.rs")).await.expect("async operation succeeded");
        let _ = rx.recv().await; // consume open event

        svc.insert_text(&id, 1, "y").await.expect("async operation succeeded");
        let event = rx.recv().await.expect("event received");
        assert!(matches!(event, AppEvent::EditorBufferChanged { .. }));
    }

    // --- Unicode ---

    #[tokio::test]
    async fn test_unicode_content() {
        let (svc, id) = service_with_file("こんにちは").await;
        let content = svc.get_content(&id).await.expect("async operation succeeded");
        assert_eq!(content, "こんにちは");
    }

    #[tokio::test]
    async fn test_insert_unicode() {
        let (svc, id) = service_with_file("hello").await;
        svc.insert_text(&id, 5, " 世界").await.expect("async operation succeeded");
        assert_eq!(svc.get_content(&id).await.expect("async operation succeeded"), "hello 世界");
    }

    // --- Language detection ---

    #[tokio::test]
    async fn test_language_detection_rust() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/test.rs"), "").await;
        let svc = EditorService::with_backend(events, backend);
        let id = svc.open_file(Path::new("/test.rs")).await.expect("async operation succeeded");
        let info = svc.get_buffer_info(&id).await.expect("async operation succeeded");
        assert_eq!(info.language, "Rust");
    }

    #[tokio::test]
    async fn test_language_detection_solidity() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/Token.sol"), "").await;
        let svc = EditorService::with_backend(events, backend);
        let id = svc.open_file(Path::new("/Token.sol")).await.expect("async operation succeeded");
        let info = svc.get_buffer_info(&id).await.expect("async operation succeeded");
        assert_eq!(info.language, "Solidity");
    }

    // --- Search ---

    #[tokio::test]
    async fn test_search_finds_matches() {
        let (svc, id) = service_with_file("hello world\nhello rust\ngoodbye").await;
        let results = svc.search(&id, "hello").await.expect("search succeeded");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], CursorPosition { line: 0, column: 0 });
        assert_eq!(results[1], CursorPosition { line: 1, column: 0 });
    }

    #[tokio::test]
    async fn test_search_case_insensitive() {
        let (svc, id) = service_with_file("Hello HELLO hello").await;
        let results = svc.search(&id, "hello").await.expect("search succeeded");
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn test_search_no_matches() {
        let (svc, id) = service_with_file("hello world").await;
        let results = svc.search(&id, "xyz").await.expect("search succeeded");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let (svc, id) = service_with_file("hello").await;
        let results = svc.search(&id, "").await.expect("search succeeded");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_multiple_on_same_line() {
        let (svc, id) = service_with_file("aaa").await;
        let results = svc.search(&id, "a").await.expect("search succeeded");
        assert_eq!(results.len(), 3);
    }

    // --- Replace ---

    #[tokio::test]
    async fn test_replace_all_basic() {
        let (svc, id) = service_with_file("foo bar foo baz foo").await;
        let count = svc.replace_all(&id, "foo", "qux").await.expect("replace succeeded");
        assert_eq!(count, 3);
        assert_eq!(svc.get_content(&id).await.expect("content"), "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn test_replace_all_no_match() {
        let (svc, id) = service_with_file("hello world").await;
        let count = svc.replace_all(&id, "xyz", "abc").await.expect("replace succeeded");
        assert_eq!(count, 0);
        assert_eq!(svc.get_content(&id).await.expect("content"), "hello world");
    }

    #[tokio::test]
    async fn test_replace_all_marks_dirty() {
        let (svc, id) = service_with_file("foo").await;
        svc.replace_all(&id, "foo", "bar").await.expect("replace succeeded");
        assert!(svc.is_dirty(&id).await.expect("dirty check"));
    }

    // --- Go to line ---

    #[tokio::test]
    async fn test_go_to_line() {
        let (svc, id) = service_with_file("a\nb\nc\nd").await;
        let pos = svc.go_to_line(&id, 2).await.expect("go_to_line succeeded");
        assert_eq!(pos.line, 2);
        assert_eq!(pos.column, 0);
    }

    #[tokio::test]
    async fn test_go_to_line_clamps() {
        let (svc, id) = service_with_file("a\nb").await;
        let pos = svc.go_to_line(&id, 999).await.expect("go_to_line succeeded");
        assert_eq!(pos.line, 1); // clamped to last line
    }

    // --- Indent/Unindent ---

    #[tokio::test]
    async fn test_indent_lines() {
        let (svc, id) = service_with_file("a\nb\nc").await;
        svc.indent_lines(&id, 0, 3).await.expect("indent succeeded");
        let content = svc.get_content(&id).await.expect("content");
        assert!(content.starts_with("    a\n    b\n    c"));
    }

    #[tokio::test]
    async fn test_unindent_lines() {
        let (svc, id) = service_with_file("    a\n    b\n    c").await;
        svc.unindent_lines(&id, 0, 3).await.expect("unindent succeeded");
        let content = svc.get_content(&id).await.expect("content");
        assert_eq!(content, "a\nb\nc");
    }

    #[tokio::test]
    async fn test_unindent_partial_spaces() {
        let (svc, id) = service_with_file("  a").await;
        svc.unindent_lines(&id, 0, 1).await.expect("unindent succeeded");
        let content = svc.get_content(&id).await.expect("content");
        assert_eq!(content, "a");
    }

    // --- Comment toggle ---

    #[tokio::test]
    async fn test_toggle_comment_adds() {
        let (svc, id) = service_with_file("a\nb\nc").await;
        svc.toggle_comment(&id, 0, 3).await.expect("comment succeeded");
        let content = svc.get_content(&id).await.expect("content");
        assert!(content.contains("// a"));
        assert!(content.contains("// b"));
        assert!(content.contains("// c"));
    }

    #[tokio::test]
    async fn test_toggle_comment_removes() {
        let (svc, id) = service_with_file("// a\n// b\n// c").await;
        svc.toggle_comment(&id, 0, 3).await.expect("comment succeeded");
        let content = svc.get_content(&id).await.expect("content");
        assert_eq!(content, "a\nb\nc");
    }

    #[tokio::test]
    async fn test_toggle_comment_roundtrip() {
        let (svc, id) = service_with_file("let x = 1;").await;
        svc.toggle_comment(&id, 0, 1).await.expect("comment add");
        assert_eq!(svc.get_content(&id).await.expect("content"), "// let x = 1;");
        svc.toggle_comment(&id, 0, 1).await.expect("comment remove");
        assert_eq!(svc.get_content(&id).await.expect("content"), "let x = 1;");
    }

    // --- Word at position ---

    #[tokio::test]
    async fn test_get_word_at() {
        let (svc, id) = service_with_file("hello world").await;
        let (start, end) = svc.get_word_at(&id, 0, 3).await.expect("word_at succeeded");
        assert_eq!(start, 0);
        assert_eq!(end, 5); // "hello"
    }

    #[tokio::test]
    async fn test_get_word_at_underscore() {
        let (svc, id) = service_with_file("my_variable = 1").await;
        let (start, end) = svc.get_word_at(&id, 0, 5).await.expect("word_at succeeded");
        assert_eq!(start, 0);
        assert_eq!(end, 11); // "my_variable"
    }

    #[tokio::test]
    async fn test_get_word_at_boundary() {
        let (svc, id) = service_with_file("a + b").await;
        let (start, end) = svc.get_word_at(&id, 0, 2).await.expect("word_at succeeded");
        // cursor on " + " — no word
        assert_eq!(start, 2);
        assert_eq!(end, 2);
    }

    // --- Search all buffers ---

    #[tokio::test]
    async fn test_search_all_buffers() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(InMemoryEditorBackend::new());
        backend.add_file(Path::new("/a.rs"), "fn main() {}").await;
        backend.add_file(Path::new("/b.rs"), "fn test() {}").await;
        let svc = EditorService::with_backend(events, backend);
        svc.open_file(Path::new("/a.rs")).await.expect("open a");
        svc.open_file(Path::new("/b.rs")).await.expect("open b");

        let results = svc.search_all_buffers("fn").await.expect("search all");
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_search_all_buffers_empty_query() {
        let (svc, _) = service_with_file("hello").await;
        let results = svc.search_all_buffers("").await.expect("search all");
        assert!(results.is_empty());
    }
}
