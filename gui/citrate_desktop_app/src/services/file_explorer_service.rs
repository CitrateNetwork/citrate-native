//! File explorer service — directory traversal, gitignore-aware filtering.
//!
//! Data source: filesystem via walkdir + ignore crates.
//! Maintains a tree of expanded/collapsed directories for the UI.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::view_models::ide_view_models::{FileEntry, FileTreeNode, FileType};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Backend trait for file explorer operations.
#[async_trait::async_trait]
pub trait FileExplorerBackend: Send + Sync {
    /// List directory entries sorted: directories first, then files, alphabetical.
    async fn list_directory(&self, path: &Path) -> Result<Vec<FileEntry>, AppError>;
    /// Check if a path should be ignored (.gitignore, .git, node_modules, target).
    async fn is_ignored(&self, path: &Path, root: &Path) -> bool;
}

/// Test-only backend that returns empty directories. Not available in release builds.
#[cfg(test)]
pub struct EmptyFileExplorerBackend;

#[cfg(test)]
#[async_trait::async_trait]
impl FileExplorerBackend for EmptyFileExplorerBackend {
    async fn list_directory(&self, _path: &Path) -> Result<Vec<FileEntry>, AppError> {
        Ok(vec![])
    }
    async fn is_ignored(&self, _path: &Path, _root: &Path) -> bool {
        false
    }
}

/// Real filesystem backend using walkdir.
pub struct RealFileExplorerBackend;

#[async_trait::async_trait]
impl FileExplorerBackend for RealFileExplorerBackend {
    async fn list_directory(&self, path: &Path) -> Result<Vec<FileEntry>, AppError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let read_dir = std::fs::read_dir(&path)
                .map_err(|e| AppError::FileSystem(format!("{}: {}", path.display(), e)))?;

            let mut entries = Vec::new();
            for entry in read_dir {
                let entry = entry.map_err(|e| AppError::FileSystem(e.to_string()))?;
                let metadata = entry.metadata().map_err(|e| AppError::FileSystem(e.to_string()))?;
                let name = entry.file_name().to_string_lossy().to_string();

                // Skip hidden files/dirs
                if name.starts_with('.') {
                    continue;
                }
                // Skip common noise directories
                if metadata.is_dir() && matches!(name.as_str(), "node_modules" | "target" | "__pycache__" | ".git") {
                    continue;
                }

                entries.push(FileEntry {
                    name,
                    path: entry.path().to_string_lossy().to_string(),
                    is_directory: metadata.is_dir(),
                    size_bytes: if metadata.is_file() { metadata.len() } else { 0 },
                });
            }

            // Sort: directories first, then alphabetical
            entries.sort_by(|a, b| {
                b.is_directory
                    .cmp(&a.is_directory)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });

            Ok(entries)
        })
        .await
        .map_err(|e| AppError::FileSystem(format!("task join: {}", e)))?
    }

    async fn is_ignored(&self, path: &Path, _root: &Path) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        name.starts_with('.') || matches!(name.as_str(), "node_modules" | "target" | "__pycache__")
    }
}

/// File explorer service — manages the tree view state.
pub struct FileExplorerService {
    events: Arc<EventBus>,
    root_path: Arc<RwLock<Option<PathBuf>>>,
    expanded_dirs: Arc<RwLock<HashSet<PathBuf>>>,
    cached_nodes: Arc<RwLock<Vec<FileTreeNode>>>,
    backend: Arc<dyn FileExplorerBackend>,
}

impl FileExplorerService {
    /// Create with the real filesystem backend.
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            root_path: Arc::new(RwLock::new(None)),
            expanded_dirs: Arc::new(RwLock::new(HashSet::new())),
            cached_nodes: Arc::new(RwLock::new(Vec::new())),
            backend: Arc::new(RealFileExplorerBackend),
        }
    }

    /// Create with an injected backend (for testing or alternative storage).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn FileExplorerBackend>) -> Self {
        Self {
            events,
            root_path: Arc::new(RwLock::new(None)),
            expanded_dirs: Arc::new(RwLock::new(HashSet::new())),
            cached_nodes: Arc::new(RwLock::new(Vec::new())),
            backend,
        }
    }

    /// Set the root directory and build initial tree.
    pub async fn set_root(&self, path: &Path) -> Result<(), AppError> {
        if !path.is_dir() {
            return Err(AppError::FileSystem(format!("Not a directory: {}", path.display())));
        }
        *self.root_path.write().await = Some(path.to_path_buf());
        self.expanded_dirs.write().await.clear();
        self.expanded_dirs.write().await.insert(path.to_path_buf());
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Get the current root path.
    pub async fn get_root(&self) -> Option<PathBuf> {
        self.root_path.read().await.clone()
    }

    /// Toggle a directory expanded/collapsed.
    pub async fn toggle_directory(&self, path: &Path) -> Result<(), AppError> {
        let mut expanded = self.expanded_dirs.write().await;
        let path_buf = path.to_path_buf();
        if expanded.contains(&path_buf) {
            expanded.remove(&path_buf);
        } else {
            expanded.insert(path_buf);
        }
        drop(expanded);
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Get the flattened visible tree nodes.
    pub async fn get_tree(&self) -> Vec<FileTreeNode> {
        self.cached_nodes.read().await.clone()
    }

    /// Synchronous version for UI polling — returns empty if lock unavailable.
    pub fn get_tree_sync(&self) -> Vec<FileTreeNode> {
        self.cached_nodes.try_read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Create a new file with optional initial content.
    pub async fn create_file(&self, path: &Path, content: &str) -> Result<(), AppError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AppError::FileSystem(format!("Cannot create parent dirs: {}", e)))?;
        }
        tokio::fs::write(path, content)
            .await
            .map_err(|e| AppError::FileSystem(format!("Cannot create file: {}", e)))?;
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Create a new directory.
    pub async fn create_directory(&self, path: &Path) -> Result<(), AppError> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| AppError::FileSystem(format!("Cannot create directory: {}", e)))?;
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Delete a file or empty directory.
    pub async fn delete_path(&self, path: &Path) -> Result<(), AppError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| AppError::FileSystem(format!("Cannot stat {}: {}", path.display(), e)))?;

        if metadata.is_dir() {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|e| AppError::FileSystem(format!("Cannot delete directory: {}", e)))?;
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| AppError::FileSystem(format!("Cannot delete file: {}", e)))?;
        }
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Rename a file or directory.
    pub async fn rename_path(&self, from: &Path, to: &Path) -> Result<(), AppError> {
        tokio::fs::rename(from, to)
            .await
            .map_err(|e| AppError::FileSystem(format!("Cannot rename: {}", e)))?;
        self.rebuild_tree().await?;
        Ok(())
    }

    /// Fuzzy search for files by name within the root directory.
    pub async fn find_files(&self, query: &str) -> Result<Vec<FileTreeNode>, AppError> {
        let root = self.root_path.read().await.clone();
        let root = match root {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let query = query.to_string();
        let root_clone = root.clone();
        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            let query_lower = query.to_lowercase();
            for entry in walkdir::WalkDir::new(&root_clone)
                .max_depth(10)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if name.to_lowercase().contains(&query_lower) {
                    let is_dir = entry.file_type().is_dir();
                    let ext = entry.path().extension()
                        .map(|e| e.to_string_lossy().to_string())
                        .unwrap_or_default();
                    results.push(FileTreeNode {
                        path: entry.path().to_string_lossy().to_string(),
                        name,
                        is_directory: is_dir,
                        is_expanded: false,
                        depth: 0,
                        file_type: FileType::from_extension(&ext),
                        git_status: None,
                        size_bytes: entry.metadata().ok().map(|m| m.len()),
                    });
                }
                if results.len() >= 50 {
                    break;
                }
            }
            Ok(results)
        })
        .await
        .map_err(|e| AppError::FileSystem(format!("search task failed: {}", e)))?
    }

    /// Rebuild the flattened tree from the current expanded state.
    async fn rebuild_tree(&self) -> Result<(), AppError> {
        let root = self.root_path.read().await.clone();
        let root = match root {
            Some(r) => r,
            None => return Ok(()),
        };
        let expanded = self.expanded_dirs.read().await.clone();
        let mut nodes = Vec::new();
        self.build_tree_recursive(&root, 0, &expanded, &mut nodes).await?;
        *self.cached_nodes.write().await = nodes;
        self.events.publish(AppEvent::FileTreeChanged);
        Ok(())
    }

    /// Recursively build the tree for expanded directories.
    async fn build_tree_recursive(
        &self,
        dir_path: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        nodes: &mut Vec<FileTreeNode>,
    ) -> Result<(), AppError> {
        let is_expanded = expanded.contains(dir_path);

        // Add the directory node itself (skip root at depth 0)
        if depth > 0 {
            let name = dir_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| dir_path.to_string_lossy().to_string());
            nodes.push(FileTreeNode {
                path: dir_path.to_string_lossy().to_string(),
                name,
                is_directory: true,
                is_expanded,
                depth,
                file_type: FileType::Unknown,
                git_status: None,
                size_bytes: None,
            });
        }

        // If expanded, list children
        if is_expanded {
            let entries = self.backend.list_directory(dir_path).await?;
            for entry in entries {
                let entry_path = PathBuf::from(&entry.path);
                if entry.is_directory {
                    // Recurse into subdirectory
                    Box::pin(self.build_tree_recursive(
                        &entry_path,
                        depth + 1,
                        expanded,
                        nodes,
                    ))
                    .await?;
                } else {
                    // Add file node
                    let ext = entry_path
                        .extension()
                        .map(|e| e.to_string_lossy().to_string())
                        .unwrap_or_default();
                    nodes.push(FileTreeNode {
                        path: entry.path,
                        name: entry.name,
                        is_directory: false,
                        is_expanded: false,
                        depth: depth + 1,
                        file_type: FileType::from_extension(&ext),
                        git_status: None,
                        size_bytes: Some(entry.size_bytes),
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> FileExplorerService {
        let events = Arc::new(EventBus::new());
        FileExplorerService::with_backend(events, Arc::new(EmptyFileExplorerBackend))
    }

    #[tokio::test]
    async fn test_initial_tree_is_empty() {
        let svc = test_service();
        let tree = svc.get_tree().await;
        assert!(tree.is_empty());
    }

    #[tokio::test]
    async fn test_initial_root_is_none() {
        let svc = test_service();
        assert!(svc.get_root().await.is_none());
    }

    #[tokio::test]
    async fn test_set_root_stores_path() {
        let svc = test_service();
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("async operation succeeded");
        assert_eq!(svc.get_root().await.expect("async operation succeeded"), tmp);
    }

    #[tokio::test]
    async fn test_set_root_non_directory_fails() {
        let svc = test_service();
        let result = svc.set_root(Path::new("/nonexistent/path/that/does/not/exist")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_toggle_directory() {
        let svc = test_service();
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("async operation succeeded");

        let sub = tmp.join("test_toggle_subdir");
        let _ = std::fs::create_dir(&sub);

        // Initially not expanded
        let expanded = svc.expanded_dirs.read().await;
        assert!(!expanded.contains(&sub));
        drop(expanded);

        // Toggle expand
        svc.toggle_directory(&sub).await.expect("async operation succeeded");
        let expanded = svc.expanded_dirs.read().await;
        assert!(expanded.contains(&sub));
        drop(expanded);

        // Toggle collapse
        svc.toggle_directory(&sub).await.expect("async operation succeeded");
        let expanded = svc.expanded_dirs.read().await;
        assert!(!expanded.contains(&sub));

        let _ = std::fs::remove_dir(&sub);
    }

    #[tokio::test]
    async fn test_with_backend_constructor() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(EmptyFileExplorerBackend);
        let svc = FileExplorerService::with_backend(events, backend);
        assert!(svc.get_root().await.is_none());
    }

    #[tokio::test]
    async fn test_set_root_clears_expanded() {
        let svc = test_service();
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("async operation succeeded");
        // Root is auto-expanded
        assert!(svc.expanded_dirs.read().await.contains(&tmp));
    }

    #[tokio::test]
    async fn test_set_root_publishes_event() {
        let events = Arc::new(EventBus::new());
        let mut rx = events.subscribe();
        let svc = FileExplorerService::with_backend(events, Arc::new(EmptyFileExplorerBackend));
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("set_root succeeded");

        let event = rx.recv().await.expect("event received");
        assert!(matches!(event, AppEvent::FileTreeChanged));
    }

    #[tokio::test]
    async fn test_custom_backend_list() {
        struct MockBackend;

        #[async_trait::async_trait]
        impl FileExplorerBackend for MockBackend {
            async fn list_directory(&self, _path: &Path) -> Result<Vec<FileEntry>, AppError> {
                Ok(vec![
                    FileEntry {
                        name: "contracts".to_string(),
                        path: "/project/contracts".to_string(),
                        is_directory: true,
                        size_bytes: 0,
                    },
                    FileEntry {
                        name: "Token.sol".to_string(),
                        path: "/project/Token.sol".to_string(),
                        is_directory: false,
                        size_bytes: 1024,
                    },
                ])
            }
            async fn is_ignored(&self, _: &Path, _: &Path) -> bool { false }
        }

        let events = Arc::new(EventBus::new());
        let svc = FileExplorerService::with_backend(events, Arc::new(MockBackend));
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("async operation succeeded");

        let tree = svc.get_tree().await;
        // Root is expanded, should show children from mock
        assert!(!tree.is_empty());
    }

    #[tokio::test]
    async fn test_real_backend_lists_directory() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(RealFileExplorerBackend);
        let svc = FileExplorerService::with_backend(events, backend);

        let test_dir = std::env::temp_dir().join(format!("citrate_real_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(test_dir.join("src")).expect("create dir");
        std::fs::write(test_dir.join("src/main.rs"), "fn main() {}").expect("write");

        svc.set_root(&test_dir).await.expect("set_root succeeded");
        let tree = svc.get_tree().await;
        assert!(!tree.is_empty(), "Tree should contain the src directory");
        assert_eq!(tree[0].name, "src");
        assert!(tree[0].is_directory);

        std::fs::remove_dir_all(&test_dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_real_backend_list_directory_sort_order() {
        let backend = RealFileExplorerBackend;
        // Create a controlled test directory with known contents
        let test_dir = std::env::temp_dir().join(format!("citrate_sort_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(test_dir.join("alpha_dir")).expect("create dir");
        std::fs::create_dir_all(test_dir.join("beta_dir")).expect("create dir");
        std::fs::write(test_dir.join("charlie.txt"), "c").expect("write file");
        std::fs::write(test_dir.join("delta.txt"), "d").expect("write file");

        let entries = backend.list_directory(&test_dir).await.expect("list dir succeeded");
        assert_eq!(entries.len(), 4);
        // Directories should come first
        assert!(entries[0].is_directory, "First entry should be a dir");
        assert!(entries[1].is_directory, "Second entry should be a dir");
        assert!(!entries[2].is_directory, "Third entry should be a file");
        assert!(!entries[3].is_directory, "Fourth entry should be a file");
        // Alphabetical within groups
        assert_eq!(entries[0].name, "alpha_dir");
        assert_eq!(entries[1].name, "beta_dir");
        assert_eq!(entries[2].name, "charlie.txt");
        assert_eq!(entries[3].name, "delta.txt");

        std::fs::remove_dir_all(&test_dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_real_backend_hidden_files_filtered() {
        let backend = RealFileExplorerBackend;
        let test_dir = std::env::temp_dir().join(format!("citrate_hidden_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&test_dir).expect("create dir");
        std::fs::write(test_dir.join("visible.txt"), "v").expect("write");
        std::fs::write(test_dir.join(".hidden"), "h").expect("write");

        let entries = backend.list_directory(&test_dir).await.expect("list dir succeeded");
        assert_eq!(entries.len(), 1, "Only visible file should be returned");
        assert_eq!(entries[0].name, "visible.txt");

        std::fs::remove_dir_all(&test_dir).expect("cleanup");
    }

    #[tokio::test]
    async fn test_file_tree_node_depth() {
        struct NestedBackend;

        #[async_trait::async_trait]
        impl FileExplorerBackend for NestedBackend {
            async fn list_directory(&self, path: &Path) -> Result<Vec<FileEntry>, AppError> {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name == "src" {
                    Ok(vec![FileEntry {
                        name: "main.rs".to_string(),
                        path: format!("{}/main.rs", path.display()),
                        is_directory: false,
                        size_bytes: 256,
                    }])
                } else {
                    Ok(vec![FileEntry {
                        name: "src".to_string(),
                        path: format!("{}/src", path.display()),
                        is_directory: true,
                        size_bytes: 0,
                    }])
                }
            }
            async fn is_ignored(&self, _: &Path, _: &Path) -> bool { false }
        }

        let events = Arc::new(EventBus::new());
        let svc = FileExplorerService::with_backend(events, Arc::new(NestedBackend));
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("async operation succeeded");

        let tree = svc.get_tree().await;
        // Should have: src (depth 1, collapsed)
        assert!(!tree.is_empty());
        assert_eq!(tree[0].name, "src");
        assert_eq!(tree[0].depth, 1);
        assert!(tree[0].is_directory);
    }

    // --- File operations ---

    #[tokio::test]
    async fn test_create_file() {
        let svc = test_service();
        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("create test dir");

        let file_path = tmp.join("test.sol");
        let result = svc.create_file(&file_path, "pragma solidity ^0.8.0;").await;
        assert!(result.is_ok());
        assert!(file_path.exists());
        let content = std::fs::read_to_string(&file_path).expect("read file");
        assert_eq!(content, "pragma solidity ^0.8.0;");

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_create_directory() {
        let svc = test_service();
        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));

        let dir_path = tmp.join("contracts");
        let result = svc.create_directory(&dir_path).await;
        assert!(result.is_ok());
        assert!(dir_path.is_dir());

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_delete_file() {
        let svc = test_service();
        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("create test dir");
        let file_path = tmp.join("to_delete.txt");
        std::fs::write(&file_path, "delete me").expect("write file");

        let result = svc.delete_path(&file_path).await;
        assert!(result.is_ok());
        assert!(!file_path.exists());

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_delete_directory() {
        let svc = test_service();
        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));
        let sub = tmp.join("subdir");
        std::fs::create_dir_all(&sub).expect("create dirs");
        std::fs::write(sub.join("file.txt"), "content").expect("write");

        let result = svc.delete_path(&sub).await;
        assert!(result.is_ok());
        assert!(!sub.exists());

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_rename_file() {
        let svc = test_service();
        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("create test dir");
        let old_path = tmp.join("old.txt");
        let new_path = tmp.join("new.txt");
        std::fs::write(&old_path, "content").expect("write");

        let result = svc.rename_path(&old_path, &new_path).await;
        assert!(result.is_ok());
        assert!(!old_path.exists());
        assert!(new_path.exists());

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_find_files() {
        let events = Arc::new(EventBus::new());
        let backend = Arc::new(RealFileExplorerBackend);
        let svc = FileExplorerService::with_backend(events, backend);

        let tmp = std::env::temp_dir().join(format!("citrate_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(tmp.join("src")).expect("create dirs");
        std::fs::write(tmp.join("src/main.rs"), "fn main() {}").expect("write");
        std::fs::write(tmp.join("src/lib.rs"), "pub mod foo;").expect("write");
        std::fs::write(tmp.join("README.md"), "# hello").expect("write");

        svc.set_root(&tmp).await.expect("set root");
        let results = svc.find_files("main").await.expect("find files");
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.name == "main.rs"));

        std::fs::remove_dir_all(&tmp).expect("cleanup");
    }

    #[tokio::test]
    async fn test_find_files_empty_query() {
        let svc = test_service();
        let tmp = std::env::temp_dir();
        svc.set_root(&tmp).await.expect("set root");
        let results = svc.find_files("").await.expect("find files");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_fails() {
        let svc = test_service();
        let result = svc.delete_path(Path::new("/nonexistent/file.txt")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rename_nonexistent_fails() {
        let svc = test_service();
        let result = svc.rename_path(
            Path::new("/nonexistent/old.txt"),
            Path::new("/nonexistent/new.txt"),
        ).await;
        assert!(result.is_err());
    }
}
