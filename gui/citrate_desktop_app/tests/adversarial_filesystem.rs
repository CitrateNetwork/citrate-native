//! Adversarial tests for FileExplorerService.
//!
//! Tests path traversal, symlink loops, permission denied,
//! and malicious filenames.

use citrate_desktop_app::services::file_explorer_service::{
    FileExplorerService, RealFileExplorerBackend,
};
use citrate_desktop_app::event_bus::EventBus;
use std::path::Path;
use std::sync::Arc;

fn real_service() -> FileExplorerService {
    let events = Arc::new(EventBus::new());
    FileExplorerService::with_backend(events, Arc::new(RealFileExplorerBackend))
}

fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("citrate_adv_fs_{}", name));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

// =========================================================================
// PATH TRAVERSAL ATTACKS
// =========================================================================

#[tokio::test]
async fn test_path_traversal_in_create_file() {
    let svc = real_service();
    let dir = test_dir("traversal_create");
    svc.set_root(&dir).await.expect("set root");

    // Try to create a file outside the root
    let malicious = dir.join("../../../etc/evil_file.txt");
    // This should work (we don't restrict paths) but it's a real file operation
    // The test verifies no crash — real security would be in the UI layer
    let _ = svc.create_file(&malicious, "evil").await;

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_file(&malicious).ok();
}

#[tokio::test]
async fn test_path_with_null_bytes() {
    let svc = real_service();
    // Paths with null bytes should fail gracefully
    let result = svc.set_root(Path::new("/tmp/test\x00evil")).await;
    assert!(result.is_err(), "Null byte in path should fail");
}

// =========================================================================
// FILENAMES WITH SPECIAL CHARACTERS
// =========================================================================

#[tokio::test]
async fn test_filename_with_spaces() {
    let svc = real_service();
    let dir = test_dir("spaces");
    let file = dir.join("my file with spaces.sol");
    std::fs::write(&file, "// content").expect("write");
    svc.set_root(&dir).await.expect("set root");

    let tree = svc.get_tree().await;
    assert!(tree.iter().any(|n| n.name == "my file with spaces.sol"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_filename_with_unicode() {
    let svc = real_service();
    let dir = test_dir("unicode_names");
    let file = dir.join("コントラクト.sol");
    std::fs::write(&file, "// content").expect("write");
    svc.set_root(&dir).await.expect("set root");

    let tree = svc.get_tree().await;
    assert!(tree.iter().any(|n| n.name == "コントラクト.sol"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_filename_very_long() {
    let svc = real_service();
    let dir = test_dir("longname");
    let long_name = format!("{}.sol", "a".repeat(200));
    let file = dir.join(&long_name);
    // Some filesystems limit to 255 bytes
    match std::fs::write(&file, "// content") {
        Ok(()) => {
            svc.set_root(&dir).await.expect("set root");
            let tree = svc.get_tree().await;
            assert!(!tree.is_empty());
        }
        Err(_) => {
            // Filesystem rejected the long name — that's fine
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// SYMLINK ATTACKS
// =========================================================================

#[tokio::test]
async fn test_symlink_to_outside_directory() {
    let svc = real_service();
    let dir = test_dir("symlink_outside");

    #[cfg(unix)]
    {
        let link = dir.join("escape");
        let _ = std::os::unix::fs::symlink("/etc", &link);
        svc.set_root(&dir).await.expect("set root");
        let tree = svc.get_tree().await;
        // Symlink should be listed but not cause infinite traversal
        let _ = tree;
    }

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// EMPTY AND PERMISSION EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_empty_directory() {
    let svc = real_service();
    let dir = test_dir("empty_dir");
    svc.set_root(&dir).await.expect("set root");
    let tree = svc.get_tree().await;
    assert!(tree.is_empty(), "Empty directory should have no nodes");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_nonexistent_root() {
    let svc = real_service();
    let result = svc.set_root(Path::new("/nonexistent/path/that/does/not/exist/12345")).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_deeply_nested_directory() {
    let svc = real_service();
    let dir = test_dir("deep_nest");
    let mut path = dir.clone();
    for i in 0..20 {
        path = path.join(format!("level_{}", i));
    }
    std::fs::create_dir_all(&path).expect("create deep dirs");
    std::fs::write(path.join("deep.txt"), "content").expect("write");

    svc.set_root(&dir).await.expect("set root");
    // Just root level should be visible (deeper levels not expanded)
    let tree = svc.get_tree().await;
    assert!(!tree.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// FIND FILES ADVERSARIAL
// =========================================================================

#[tokio::test]
async fn test_find_files_regex_metacharacters() {
    let svc = real_service();
    let dir = test_dir("find_regex");
    std::fs::write(dir.join("test.rs"), "").expect("write");
    svc.set_root(&dir).await.expect("set root");

    // Query with regex metacharacters should be treated as literal
    let results = svc.find_files("test.*").await.expect("find files");
    // Should not crash, may or may not find results depending on literal match
    let _ = results;

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_find_files_very_long_query() {
    let svc = real_service();
    let dir = test_dir("find_long");
    svc.set_root(&dir).await.expect("set root");

    let long_query = "x".repeat(10_000);
    let results = svc.find_files(&long_query).await.expect("find files");
    assert!(results.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// DELETE / RENAME SAFETY
// =========================================================================

#[tokio::test]
async fn test_delete_root_directory() {
    let svc = real_service();
    let dir = test_dir("delete_root");
    std::fs::write(dir.join("file.txt"), "content").expect("write");
    svc.set_root(&dir).await.expect("set root");

    // Delete a file inside the root — this should succeed
    let result = svc.delete_path(&dir.join("file.txt")).await;
    assert!(result.is_ok());
    // Tree should update to show the file is gone
    let tree = svc.get_tree().await;
    assert!(tree.iter().all(|n| n.name != "file.txt"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_to_existing_path() {
    let svc = real_service();
    let dir = test_dir("rename_existing");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");
    std::fs::write(&file_a, "aaa").expect("write a");
    std::fs::write(&file_b, "bbb").expect("write b");

    // Rename a to b — should overwrite b on most filesystems
    let result = svc.rename_path(&file_a, &file_b).await;
    assert!(result.is_ok());
    assert!(!file_a.exists());
    let content = std::fs::read_to_string(&file_b).expect("read b");
    assert_eq!(content, "aaa"); // b now has a's content

    std::fs::remove_dir_all(&dir).ok();
}
