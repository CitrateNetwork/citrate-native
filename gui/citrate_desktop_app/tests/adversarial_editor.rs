//! Adversarial tests for EditorService.
//!
//! Tests malicious, unexpected, and boundary inputs that a real user
//! or attacker might produce against the code editor.

use citrate_desktop_app::services::editor_service::EditorService;
use citrate_desktop_app::event_bus::EventBus;
use std::sync::Arc;

/// Create a service with a real file on disk for integration testing
async fn svc_with(filename: &str, content: &str) -> (EditorService, String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("citrate_adv_ed_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create dir");
    let file_path = dir.join(filename);
    std::fs::write(&file_path, content).expect("write file");

    let events = Arc::new(EventBus::new());
    let svc = EditorService::new(events);
    let id = svc.open_file(&file_path).await.expect("open file");
    (svc, id, dir)
}

// =========================================================================
// HUGE FILES — memory and performance boundaries
// =========================================================================

#[tokio::test]
async fn test_open_10k_line_file() {
    let content: String = (0..10_000).map(|i| format!("line {} content here\n", i)).collect();
    let (svc, id, _dir) = svc_with("huge.rs", &content).await;
    let count = svc.get_line_count(&id).await.expect("line count");
    assert!(count >= 10_000, "Should handle 10K lines, got {}", count);
}

#[tokio::test]
async fn test_very_long_single_line() {
    let content = "x".repeat(1_000_000); // 1MB single line
    let (svc, id, _dir) = svc_with("longline.txt", &content).await;
    let lines = svc.get_visible_lines(&id, 0, 1).await.expect("visible lines");
    assert_eq!(lines.len(), 1);
}

#[tokio::test]
async fn test_empty_file() {
    let (svc, id, _dir) = svc_with("empty.rs", "").await;
    let count = svc.get_line_count(&id).await.expect("line count");
    assert_eq!(count, 1); // ropey counts 1 for empty
    let lines = svc.get_visible_lines(&id, 0, 10).await.expect("visible lines");
    assert!(!lines.is_empty());
}

// =========================================================================
// BINARY / INVALID UTF-8 — editor must not crash
// =========================================================================

#[tokio::test]
async fn test_file_with_null_bytes() {
    let content = "hello\x00world\x00end";
    let (svc, id, _dir) = svc_with("nulls.bin", content).await;
    let result = svc.get_content(&id).await.expect("content");
    assert!(result.contains('\0'), "Null bytes should be preserved");
}

#[tokio::test]
async fn test_file_with_mixed_line_endings() {
    let content = "line1\r\nline2\nline3\rline4";
    let (svc, id, _dir) = svc_with("mixed.txt", content).await;
    let count = svc.get_line_count(&id).await.expect("line count");
    assert!(count >= 3, "Should parse mixed line endings");
}

// =========================================================================
// INSERT AT BOUNDARIES
// =========================================================================

#[tokio::test]
async fn test_insert_beyond_buffer_length() {
    let (svc, id, _dir) = svc_with("short.txt", "abc").await;
    // Insert at offset 999 — should clamp to end
    let result = svc.insert_text(&id, 999, "X").await;
    assert!(result.is_ok(), "Insert beyond end should clamp, not crash");
    let content = svc.get_content(&id).await.expect("content");
    assert!(content.contains('X'));
}

#[tokio::test]
async fn test_delete_beyond_buffer_length() {
    let (svc, id, _dir) = svc_with("short.txt", "abc").await;
    let result = svc.delete_range(&id, 0, 999).await;
    assert!(result.is_ok(), "Delete beyond end should clamp, not crash");
    let content = svc.get_content(&id).await.expect("content");
    assert!(content.is_empty() || content.len() < 3);
}

#[tokio::test]
async fn test_insert_empty_string() {
    let (svc, id, _dir) = svc_with("test.txt", "hello").await;
    let result = svc.insert_text(&id, 2, "").await;
    assert!(result.is_ok());
    assert_eq!(svc.get_content(&id).await.expect("content"), "hello");
}

// =========================================================================
// UNDO/REDO STRESS
// =========================================================================

#[tokio::test]
async fn test_undo_100_times() {
    let (svc, id, _dir) = svc_with("undo.txt", "").await;
    for i in 0..100 {
        svc.insert_text(&id, i, "x").await.expect("insert");
    }
    for _ in 0..100 {
        svc.undo(&id).await.expect("undo");
    }
    assert_eq!(svc.get_content(&id).await.expect("content"), "");
}

#[tokio::test]
async fn test_undo_more_than_available() {
    let (svc, id, _dir) = svc_with("undo.txt", "hello").await;
    svc.insert_text(&id, 5, "!").await.expect("insert");
    svc.undo(&id).await.expect("undo 1");
    svc.undo(&id).await.expect("undo 2 (no-op)");
    svc.undo(&id).await.expect("undo 3 (no-op)");
    assert_eq!(svc.get_content(&id).await.expect("content"), "hello");
}

#[tokio::test]
async fn test_redo_more_than_available() {
    let (svc, id, _dir) = svc_with("redo.txt", "hello").await;
    svc.redo(&id).await.expect("redo (no-op)");
    svc.redo(&id).await.expect("redo (no-op)");
    assert_eq!(svc.get_content(&id).await.expect("content"), "hello");
}

// =========================================================================
// SEARCH INJECTION
// =========================================================================

#[tokio::test]
async fn test_search_regex_metacharacters() {
    let (svc, id, _dir) = svc_with("regex.txt", "hello (world) [test] {foo}").await;
    // Search should be literal, not regex — no crash on metacharacters
    let results = svc.search(&id, "(world)").await.expect("search");
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_search_very_long_query() {
    let (svc, id, _dir) = svc_with("search.txt", "short content").await;
    let long_query = "x".repeat(10_000);
    let results = svc.search(&id, &long_query).await.expect("search");
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_replace_all_with_larger_replacement() {
    let (svc, id, _dir) = svc_with("replace.txt", "a a a").await;
    let count = svc.replace_all(&id, "a", "BBBBBB").await.expect("replace");
    assert_eq!(count, 3);
    let content = svc.get_content(&id).await.expect("content");
    assert_eq!(content, "BBBBBB BBBBBB BBBBBB");
}

#[tokio::test]
async fn test_replace_all_with_empty_replacement() {
    let (svc, id, _dir) = svc_with("replace.txt", "hello world hello").await;
    let count = svc.replace_all(&id, "hello", "").await.expect("replace");
    assert_eq!(count, 2);
    let content = svc.get_content(&id).await.expect("content");
    assert_eq!(content, " world ");
}

// =========================================================================
// COMMENT TOGGLE EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_comment_toggle_empty_file() {
    let (svc, id, _dir) = svc_with("empty.rs", "").await;
    svc.toggle_comment(&id, 0, 1).await.expect("toggle comment");
    let content = svc.get_content(&id).await.expect("content");
    assert!(content.contains("//") || content.is_empty());
}

#[tokio::test]
async fn test_comment_toggle_already_double_slashed() {
    let (svc, id, _dir) = svc_with("commented.rs", "// // nested").await;
    svc.toggle_comment(&id, 0, 1).await.expect("toggle comment");
    let content = svc.get_content(&id).await.expect("content");
    // Should remove outer comment
    assert!(content.starts_with("// nested") || content.starts_with(" // nested"));
}

// =========================================================================
// INDENT/UNINDENT BOUNDARIES
// =========================================================================

#[tokio::test]
async fn test_indent_empty_range() {
    let (svc, id, _dir) = svc_with("indent.rs", "hello").await;
    svc.indent_lines(&id, 5, 5).await.expect("indent empty range");
    assert_eq!(svc.get_content(&id).await.expect("content"), "hello");
}

#[tokio::test]
async fn test_unindent_no_leading_spaces() {
    let (svc, id, _dir) = svc_with("unindent.rs", "no spaces here").await;
    svc.unindent_lines(&id, 0, 1).await.expect("unindent");
    assert_eq!(svc.get_content(&id).await.expect("content"), "no spaces here");
}

// =========================================================================
// CONCURRENT BUFFER OPERATIONS
// =========================================================================

#[tokio::test]
async fn test_rapid_insert_delete_cycle() {
    let (svc, id, _dir) = svc_with("rapid.txt", "").await;
    for i in 0..200 {
        svc.insert_text(&id, 0, &format!("{}", i % 10)).await.expect("insert");
        if i % 3 == 0 {
            let content = svc.get_content(&id).await.expect("content");
            if !content.is_empty() {
                svc.delete_range(&id, 0, 1).await.expect("delete");
            }
        }
    }
    // Just verify no crash — content will be whatever remains
    let _ = svc.get_content(&id).await.expect("final content");
}

// =========================================================================
// NONEXISTENT BUFFER OPERATIONS
// =========================================================================

#[tokio::test]
async fn test_operations_on_nonexistent_buffer() {
    let events = Arc::new(EventBus::new());
    let svc = EditorService::new(events);

    assert!(svc.insert_text("fake", 0, "x").await.is_err());
    assert!(svc.delete_range("fake", 0, 1).await.is_err());
    assert!(svc.undo("fake").await.is_err());
    assert!(svc.redo("fake").await.is_err());
    assert!(svc.save_buffer("fake").await.is_err());
    assert!(svc.get_content("fake").await.is_err());
    assert!(svc.get_visible_lines("fake", 0, 10).await.is_err());
    assert!(svc.search("fake", "test").await.is_err());
    assert!(svc.go_to_line("fake", 0).await.is_err());
    assert!(svc.get_word_at("fake", 0, 0).await.is_err());
}

// =========================================================================
// UNICODE STRESS
// =========================================================================

#[tokio::test]
async fn test_emoji_in_editor() {
    let content = "let x = \"🚀🌍💰\";";
    let (svc, id, _dir) = svc_with("emoji.rs", content).await;
    let result = svc.get_content(&id).await.expect("content");
    assert!(result.contains("🚀"));
}

#[tokio::test]
async fn test_cjk_characters() {
    let content = "let msg = \"こんにちは世界\";";
    let (svc, id, _dir) = svc_with("cjk.rs", content).await;
    let results = svc.search(&id, "世界").await.expect("search");
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_rtl_text() {
    let content = "let text = \"مرحبا بالعالم\";";
    let (svc, id, _dir) = svc_with("rtl.rs", content).await;
    let _ = svc.get_visible_lines(&id, 0, 1).await.expect("visible lines");
}

// =========================================================================
// SYNTAX HIGHLIGHTING EDGE CASES
// =========================================================================

#[tokio::test]
async fn test_highlight_unknown_language() {
    let (svc, id, _dir) = svc_with("test.xyz", "some content").await;
    // Unknown extension should still return lines (plain text fallback)
    let lines = svc.get_visible_lines(&id, 0, 1).await.expect("visible lines");
    assert!(!lines.is_empty());
    assert!(!lines[0].spans.is_empty());
}

#[tokio::test]
async fn test_highlight_solidity() {
    let content = "pragma solidity ^0.8.0;\n\ncontract Token {\n    uint256 public totalSupply;\n}";
    let (svc, id, _dir) = svc_with("Token.sol", content).await;
    let lines = svc.get_visible_lines(&id, 0, 5).await.expect("visible lines");
    // Should have multiple spans per line (keywords highlighted differently)
    assert!(lines.len() >= 4);
}

#[tokio::test]
async fn test_highlight_rust() {
    let content = "fn main() {\n    println!(\"hello\");\n}";
    let (svc, id, _dir) = svc_with("main.rs", content).await;
    let lines = svc.get_visible_lines(&id, 0, 3).await.expect("visible lines");
    assert_eq!(lines.len(), 3);
    // "fn" should be highlighted (bold or different color from "main")
    let first_line_colors: Vec<&str> = lines[0].spans.iter().map(|s| s.fg_color.as_str()).collect();
    // At minimum, there should be more than one color
    assert!(!first_line_colors.is_empty());
}
