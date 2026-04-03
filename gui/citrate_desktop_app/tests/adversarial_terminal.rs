//! Adversarial tests for TerminalService.
//!
//! Tests ANSI bombs, escape injection, boundary conditions,
//! and malicious PTY output that could crash the terminal renderer.

use citrate_desktop_app::services::terminal_service::{
    InMemoryTerminalBackend, TerminalService,
};
use citrate_desktop_app::view_models::ide_view_models::TerminalConfig;
use citrate_desktop_app::event_bus::EventBus;
use std::sync::Arc;

fn make_service() -> TerminalService {
    let events = Arc::new(EventBus::new());
    let backend = Arc::new(InMemoryTerminalBackend::new());
    TerminalService::with_backend(events, backend)
}

fn small_config() -> TerminalConfig {
    TerminalConfig {
        cols: 20,
        rows: 5,
        ..TerminalConfig::default()
    }
}

async fn svc_with_session() -> (TerminalService, String) {
    let svc = make_service();
    let config = small_config();
    let id = svc.create_session(&config).await.expect("create session");
    (svc, id)
}

// =========================================================================
// ANSI BOMB — huge amounts of escape sequences
// =========================================================================

#[tokio::test]
async fn test_ansi_color_flood() {
    let (svc, id) = svc_with_session().await;
    // 10000 SGR color changes
    let mut data = Vec::new();
    for i in 0..10_000u32 {
        let color = (i % 256) as u8;
        data.extend_from_slice(format!("\x1b[38;5;{}m", color).as_bytes());
        data.push(b'X');
    }
    svc.feed_bytes(&id, &data).await.expect("feed ANSI flood");
    let grid = svc.get_grid(&id).await.expect("get grid after flood");
    // Grid should still be valid
    assert_eq!(grid.rows as usize, grid.cells.len());
}

#[tokio::test]
async fn test_cursor_movement_flood() {
    let (svc, id) = svc_with_session().await;
    // Rapid cursor movements: up, down, left, right repeated
    let mut data = Vec::new();
    for _ in 0..5_000 {
        data.extend_from_slice(b"\x1b[A\x1b[B\x1b[C\x1b[D");
    }
    svc.feed_bytes(&id, &data).await.expect("feed cursor flood");
    let grid = svc.get_grid(&id).await.expect("get grid");
    assert!(grid.cursor_row < grid.rows as usize);
    assert!(grid.cursor_col < grid.cols as usize);
}

#[tokio::test]
async fn test_erase_display_flood() {
    let (svc, id) = svc_with_session().await;
    let mut data = Vec::new();
    for _ in 0..1_000 {
        data.extend_from_slice(b"\x1b[2J"); // clear entire screen
    }
    svc.feed_bytes(&id, &data).await.expect("feed erase flood");
    let grid = svc.get_grid(&id).await.expect("get grid");
    // All cells should be empty after clear
    for row in &grid.cells {
        for cell in row {
            assert_eq!(cell.character, ' ');
        }
    }
}

// =========================================================================
// MALFORMED ESCAPE SEQUENCES
// =========================================================================

#[tokio::test]
async fn test_incomplete_escape_sequence() {
    let (svc, id) = svc_with_session().await;
    // ESC without completing the sequence
    svc.feed_bytes(&id, b"\x1b").await.expect("incomplete ESC");
    svc.feed_bytes(&id, b"[").await.expect("incomplete CSI");
    svc.feed_bytes(&id, b"A").await.expect("complete CSI A");
    let _ = svc.get_grid(&id).await.expect("grid after incomplete");
}

#[tokio::test]
async fn test_invalid_csi_parameters() {
    let (svc, id) = svc_with_session().await;
    // Invalid CSI parameters (non-numeric, too many)
    svc.feed_bytes(&id, b"\x1b[999999A").await.expect("huge param");
    svc.feed_bytes(&id, b"\x1b[;;;;;m").await.expect("many semicolons");
    svc.feed_bytes(&id, b"\x1b[?25h").await.expect("DEC private mode");
    let _ = svc.get_grid(&id).await.expect("grid after invalid CSI");
}

#[tokio::test]
async fn test_bare_escape_followed_by_text() {
    let (svc, id) = svc_with_session().await;
    svc.feed_bytes(&id, b"\x1bHello").await.expect("ESC then text");
    let grid = svc.get_grid(&id).await.expect("grid");
    // 'H' might be interpreted as ESC H (set tab) or eaten; subsequent text should appear
    // Key thing: no crash
    assert_eq!(grid.rows as usize, grid.cells.len());
}

// =========================================================================
// CURSOR OUT OF BOUNDS
// =========================================================================

#[tokio::test]
async fn test_cursor_move_up_from_top() {
    let (svc, id) = svc_with_session().await;
    // Move up 1000 rows from row 0 — should clamp to 0
    svc.feed_bytes(&id, b"\x1b[1000A").await.expect("cursor up from top");
    let grid = svc.get_grid(&id).await.expect("grid");
    assert_eq!(grid.cursor_row, 0);
}

#[tokio::test]
async fn test_cursor_move_right_beyond_width() {
    let (svc, id) = svc_with_session().await;
    // Move right 1000 cols from col 0 — should clamp to cols-1
    svc.feed_bytes(&id, b"\x1b[1000C").await.expect("cursor right beyond");
    let grid = svc.get_grid(&id).await.expect("grid");
    assert!(grid.cursor_col < grid.cols as usize);
}

#[tokio::test]
async fn test_cursor_position_beyond_grid() {
    let (svc, id) = svc_with_session().await;
    // CUP (set cursor position) to row 999, col 999
    svc.feed_bytes(&id, b"\x1b[999;999H").await.expect("CUP beyond grid");
    let grid = svc.get_grid(&id).await.expect("grid");
    assert!(grid.cursor_row < grid.rows as usize);
    assert!(grid.cursor_col < grid.cols as usize);
}

// =========================================================================
// SCROLL STRESS
// =========================================================================

#[tokio::test]
async fn test_scroll_1000_lines() {
    let (svc, id) = svc_with_session().await;
    let mut data = Vec::new();
    for i in 0..1_000 {
        data.extend_from_slice(format!("line {}\r\n", i).as_bytes());
    }
    svc.feed_bytes(&id, &data).await.expect("scroll 1000 lines");
    let grid = svc.get_grid(&id).await.expect("grid");
    // Should still have correct grid dimensions
    assert_eq!(grid.cells.len(), grid.rows as usize);
    assert_eq!(grid.cells[0].len(), grid.cols as usize);
}

// =========================================================================
// RAW BINARY DATA — terminal must not crash on arbitrary bytes
// =========================================================================

#[tokio::test]
async fn test_all_byte_values() {
    let (svc, id) = svc_with_session().await;
    let data: Vec<u8> = (0..=255).collect();
    svc.feed_bytes(&id, &data).await.expect("all byte values");
    let _ = svc.get_grid(&id).await.expect("grid after all bytes");
}

#[tokio::test]
async fn test_repeated_null_bytes() {
    let (svc, id) = svc_with_session().await;
    let data = vec![0u8; 10_000];
    svc.feed_bytes(&id, &data).await.expect("null bytes");
    let _ = svc.get_grid(&id).await.expect("grid after nulls");
}

// =========================================================================
// RESIZE BOUNDARIES
// =========================================================================

#[tokio::test]
async fn test_resize_to_1x1() {
    let (svc, id) = svc_with_session().await;
    svc.resize(&id, 1, 1).await.expect("resize to 1x1");
    let grid = svc.get_grid(&id).await.expect("grid");
    assert_eq!(grid.rows, 1);
    assert_eq!(grid.cols, 1);
}

#[tokio::test]
async fn test_resize_to_very_large() {
    let (svc, id) = svc_with_session().await;
    svc.resize(&id, 500, 200).await.expect("resize to 500x200");
    let grid = svc.get_grid(&id).await.expect("grid");
    assert_eq!(grid.rows, 200);
    assert_eq!(grid.cols, 500);
}

#[tokio::test]
async fn test_rapid_resize() {
    let (svc, id) = svc_with_session().await;
    for i in 1..50u16 {
        svc.resize(&id, i * 2 + 10, i + 5).await.expect("rapid resize");
    }
    let grid = svc.get_grid(&id).await.expect("grid after rapid resize");
    assert_eq!(grid.cells.len(), grid.rows as usize);
}

// =========================================================================
// SESSION LIFECYCLE
// =========================================================================

#[tokio::test]
async fn test_operations_on_closed_session() {
    let (svc, id) = svc_with_session().await;
    svc.close_session(&id).await.expect("close session");

    assert!(svc.write_input(&id, b"test").await.is_err());
    assert!(svc.get_grid(&id).await.is_err());
    assert!(svc.resize(&id, 80, 24).await.is_err());
}

#[tokio::test]
async fn test_double_close_session() {
    let (svc, id) = svc_with_session().await;
    svc.close_session(&id).await.expect("first close");
    // Second close should either error or be a no-op — must not panic
    let _ = svc.close_session(&id).await;
}

#[tokio::test]
async fn test_many_sessions() {
    let svc = make_service();
    let config = small_config();
    let mut ids = Vec::new();
    for _ in 0..20 {
        let id = svc.create_session(&config).await.expect("create session");
        ids.push(id);
    }
    assert_eq!(svc.list_sessions().await.len(), 20);
    for id in &ids {
        svc.close_session(id).await.expect("close session");
    }
    assert_eq!(svc.list_sessions().await.len(), 0);
}

// =========================================================================
// ESCAPE INJECTION — try to break out of terminal context
// =========================================================================

#[tokio::test]
async fn test_osc_title_injection() {
    let (svc, id) = svc_with_session().await;
    // OSC 0 (set title) — should be handled, not leak to shell
    svc.feed_bytes(&id, b"\x1b]0;INJECTED TITLE\x07").await.expect("OSC title");
    let _ = svc.get_grid(&id).await.expect("grid after OSC");
}

#[tokio::test]
async fn test_osc_hyperlink_injection() {
    let (svc, id) = svc_with_session().await;
    // OSC 8 (hyperlink) — should be silently consumed
    svc.feed_bytes(&id, b"\x1b]8;;http://evil.com\x07click\x1b]8;;\x07")
        .await
        .expect("OSC hyperlink");
    let _ = svc.get_grid(&id).await.expect("grid after hyperlink");
}
