//! View model definitions for the integrated IDE.
//!
//! These structs define the contract between IDE services and the Slint UI.
//! The Rust side populates these; Slint renders them.

// ===== EDITOR =====

/// A single styled span within a line (output of syntax highlighting)
#[derive(Debug, Clone)]
pub struct StyledSpan {
    pub text: String,
    pub fg_color: String, // hex "#rrggbb"
    pub bold: bool,
    pub italic: bool,
}

/// A fully highlighted line of source code
#[derive(Debug, Clone)]
pub struct StyledLine {
    pub line_number: usize,
    pub spans: Vec<StyledSpan>,
}

/// Cursor position in the editor
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorPosition {
    pub line: usize,
    pub column: usize,
}

/// A single open buffer (tab)
#[derive(Debug, Clone)]
pub struct BufferInfo {
    pub id: String,
    pub file_path: String,
    pub file_name: String,
    pub language: String,
    pub is_dirty: bool,
    pub line_count: usize,
}

/// Diagnostic (error, warning) for problems panel
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file_path: String,
    pub line: usize,
    pub column: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

// ===== TERMINAL =====

/// A single cell in the terminal grid
#[derive(Debug, Clone)]
pub struct TerminalCell {
    pub character: char,
    pub fg_color: String,
    pub bg_color: String,
    pub bold: bool,
    pub underline: bool,
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg_color: "#f9fafb".to_string(),
            bg_color: "#0a0a1a".to_string(),
            bold: false,
            underline: false,
        }
    }
}

/// Terminal grid state — the entire visible screen
#[derive(Debug, Clone)]
pub struct TerminalGrid {
    pub cells: Vec<Vec<TerminalCell>>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub rows: u16,
    pub cols: u16,
}

/// Terminal session info for tab display
#[derive(Debug, Clone)]
pub struct TerminalSessionInfo {
    pub session_id: String,
    pub title: String,
    pub shell: String,
    pub is_active: bool,
}

/// Terminal session configuration
#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            cols: 80,
            rows: 24,
            env: vec![
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string()),
            ],
        }
    }
}

// ===== FILE EXPLORER =====

/// A single node in the file tree
#[derive(Debug, Clone)]
pub struct FileTreeNode {
    pub path: String,
    pub name: String,
    pub is_directory: bool,
    pub is_expanded: bool,
    pub depth: usize,
    pub file_type: FileType,
    pub git_status: Option<GitChangeType>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileType {
    Solidity,
    Rust,
    Toml,
    Json,
    Markdown,
    TypeScript,
    JavaScript,
    Python,
    Shell,
    Yaml,
    Unknown,
}

impl FileType {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "sol" => Self::Solidity,
            "rs" => Self::Rust,
            "toml" => Self::Toml,
            "json" => Self::Json,
            "md" => Self::Markdown,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" => Self::JavaScript,
            "py" => Self::Python,
            "sh" | "bash" | "zsh" => Self::Shell,
            "yml" | "yaml" => Self::Yaml,
            _ => Self::Unknown,
        }
    }

    pub fn language_id(&self) -> &str {
        match self {
            Self::Solidity => "Solidity",
            Self::Rust => "Rust",
            Self::Toml => "TOML",
            Self::Json => "JSON",
            Self::Markdown => "Markdown",
            Self::TypeScript => "TypeScript",
            Self::JavaScript => "JavaScript",
            Self::Python => "Python",
            Self::Shell => "Shell",
            Self::Yaml => "YAML",
            Self::Unknown => "Plain Text",
        }
    }
}

/// File entry returned by directory listing
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size_bytes: u64,
}

// ===== GIT =====

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitChangeType {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone)]
pub struct GitFileStatus {
    pub path: String,
    pub change_type: GitChangeType,
    pub staged: bool,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub content: String,
    pub line_type: DiffLineType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLineType {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub hash_short: String,
    pub message: String,
    pub author: String,
    pub timestamp: u64,
}

// ===== COMPILER =====

#[derive(Debug, Clone)]
pub struct CompileError {
    pub file_path: String,
    pub line: usize,
    pub column: usize,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CompileResult {
    pub success: bool,
    pub errors: Vec<CompileError>,
    pub warnings: Vec<CompileError>,
    pub artifacts: Vec<ContractArtifact>,
}

#[derive(Debug, Clone)]
pub struct ContractArtifact {
    pub name: String,
    pub abi_json: String,
    pub bytecode_hex: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_type_from_extension() {
        assert_eq!(FileType::from_extension("sol"), FileType::Solidity);
        assert_eq!(FileType::from_extension("rs"), FileType::Rust);
        assert_eq!(FileType::from_extension("toml"), FileType::Toml);
        assert_eq!(FileType::from_extension("json"), FileType::Json);
        assert_eq!(FileType::from_extension("md"), FileType::Markdown);
        assert_eq!(FileType::from_extension("ts"), FileType::TypeScript);
        assert_eq!(FileType::from_extension("tsx"), FileType::TypeScript);
        assert_eq!(FileType::from_extension("js"), FileType::JavaScript);
        assert_eq!(FileType::from_extension("py"), FileType::Python);
        assert_eq!(FileType::from_extension("sh"), FileType::Shell);
        assert_eq!(FileType::from_extension("yml"), FileType::Yaml);
        assert_eq!(FileType::from_extension("yaml"), FileType::Yaml);
        assert_eq!(FileType::from_extension("xyz"), FileType::Unknown);
    }

    #[test]
    fn test_file_type_case_insensitive() {
        assert_eq!(FileType::from_extension("SOL"), FileType::Solidity);
        assert_eq!(FileType::from_extension("Rs"), FileType::Rust);
    }

    #[test]
    fn test_file_type_language_id() {
        assert_eq!(FileType::Solidity.language_id(), "Solidity");
        assert_eq!(FileType::Rust.language_id(), "Rust");
        assert_eq!(FileType::Unknown.language_id(), "Plain Text");
    }

    #[test]
    fn test_terminal_cell_default() {
        let cell = TerminalCell::default();
        assert_eq!(cell.character, ' ');
        assert_eq!(cell.fg_color, "#f9fafb");
        assert_eq!(cell.bg_color, "#0a0a1a");
        assert!(!cell.bold);
        assert!(!cell.underline);
    }

    #[test]
    fn test_terminal_config_default() {
        let config = TerminalConfig::default();
        assert_eq!(config.cols, 80);
        assert_eq!(config.rows, 24);
        assert!(config.shell.is_none());
        assert!(config.cwd.is_none());
        assert!(!config.env.is_empty());
    }

    #[test]
    fn test_cursor_position_default() {
        let pos = CursorPosition::default();
        assert_eq!(pos.line, 0);
        assert_eq!(pos.column, 0);
    }

    #[test]
    fn test_cursor_position_equality() {
        let a = CursorPosition {
            line: 5,
            column: 10,
        };
        let b = CursorPosition {
            line: 5,
            column: 10,
        };
        let c = CursorPosition {
            line: 5,
            column: 11,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_styled_span_construction() {
        let span = StyledSpan {
            text: "fn".to_string(),
            fg_color: "#c678dd".to_string(),
            bold: true,
            italic: false,
        };
        assert_eq!(span.text, "fn");
        assert!(span.bold);
    }

    #[test]
    fn test_styled_line_construction() {
        let line = StyledLine {
            line_number: 1,
            spans: vec![
                StyledSpan {
                    text: "fn ".to_string(),
                    fg_color: "#c678dd".to_string(),
                    bold: true,
                    italic: false,
                },
                StyledSpan {
                    text: "main".to_string(),
                    fg_color: "#61afef".to_string(),
                    bold: false,
                    italic: false,
                },
            ],
        };
        assert_eq!(line.line_number, 1);
        assert_eq!(line.spans.len(), 2);
    }

    #[test]
    fn test_buffer_info_construction() {
        let info = BufferInfo {
            id: "buf-1".to_string(),
            file_path: "/home/user/project/main.rs".to_string(),
            file_name: "main.rs".to_string(),
            language: "Rust".to_string(),
            is_dirty: false,
            line_count: 100,
        };
        assert_eq!(info.file_name, "main.rs");
        assert!(!info.is_dirty);
    }

    #[test]
    fn test_diagnostic_severity() {
        assert_eq!(DiagnosticSeverity::Error, DiagnosticSeverity::Error);
        assert_ne!(DiagnosticSeverity::Error, DiagnosticSeverity::Warning);
    }

    #[test]
    fn test_git_change_type() {
        assert_eq!(GitChangeType::Modified, GitChangeType::Modified);
        assert_ne!(GitChangeType::Modified, GitChangeType::Added);
    }

    #[test]
    fn test_diff_line_type() {
        assert_eq!(DiffLineType::Addition, DiffLineType::Addition);
        assert_ne!(DiffLineType::Addition, DiffLineType::Deletion);
    }

    #[test]
    fn test_file_tree_node_construction() {
        let node = FileTreeNode {
            path: "/project/src".to_string(),
            name: "src".to_string(),
            is_directory: true,
            is_expanded: false,
            depth: 1,
            file_type: FileType::Unknown,
            git_status: None,
            size_bytes: None,
        };
        assert!(node.is_directory);
        assert!(!node.is_expanded);
        assert_eq!(node.depth, 1);
    }

    #[test]
    fn test_compile_result_success() {
        let result = CompileResult {
            success: true,
            errors: vec![],
            warnings: vec![],
            artifacts: vec![ContractArtifact {
                name: "Token".to_string(),
                abi_json: "[]".to_string(),
                bytecode_hex: "0x".to_string(),
            }],
        };
        assert!(result.success);
        assert_eq!(result.artifacts.len(), 1);
    }

    #[test]
    fn test_compile_result_with_errors() {
        let result = CompileResult {
            success: false,
            errors: vec![CompileError {
                file_path: "Token.sol".to_string(),
                line: 10,
                column: 5,
                severity: "error".to_string(),
                message: "undeclared identifier".to_string(),
            }],
            warnings: vec![],
            artifacts: vec![],
        };
        assert!(!result.success);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_terminal_grid_construction() {
        let grid = TerminalGrid {
            cells: vec![vec![TerminalCell::default(); 80]; 24],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            rows: 24,
            cols: 80,
        };
        assert_eq!(grid.cells.len(), 24);
        assert_eq!(grid.cells[0].len(), 80);
    }

    #[test]
    fn test_branch_info() {
        let branch = BranchInfo {
            name: "main".to_string(),
            is_current: true,
            is_remote: false,
        };
        assert!(branch.is_current);
        assert!(!branch.is_remote);
    }

    #[test]
    fn test_commit_info() {
        let commit = CommitInfo {
            hash_short: "abc1234".to_string(),
            message: "Initial commit".to_string(),
            author: "Larry".to_string(),
            timestamp: 1711555200,
        };
        assert_eq!(commit.hash_short, "abc1234");
    }
}
