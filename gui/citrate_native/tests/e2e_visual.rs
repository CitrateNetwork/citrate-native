//! End-to-end visual tests for the Citrate desktop GUI.
//!
//! These tests instantiate the full Slint App component headlessly using the
//! software renderer, navigate between tabs, verify UI state via properties,
//! and capture PNG screenshots as proof artifacts for auditors.
//!
//! Run with: cargo test -p citrate-native --test e2e_visual -- --nocapture
//!
//! Screenshots are saved to tests/screenshots/ relative to the crate root.
//!
//! NOTE: Winit event loop can only be created once per process, so all tests
//! share a single App instance in one test function. Each section is a named
//! "check" that captures a screenshot and asserts properties.

slint::include_modules!();

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Once;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::PhysicalSize;

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
}

struct TestPlatform;

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(WINDOW.with(|window| window.clone()))
    }
}

fn init_test_platform() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(err) = slint::platform::set_platform(Box::new(TestPlatform)) {
            panic!("test platform should initialize once: {err}");
        }
    });
}

fn set_window_size(width: u32, height: u32) {
    WINDOW.with(|window| {
        window.set_size(PhysicalSize::new(width, height));
    });
}

fn screenshots_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/screenshots");
    std::fs::create_dir_all(&dir).expect("create screenshots dir");
    dir
}

fn save_snapshot(app: &App, name: &str) {
    slint::platform::update_timers_and_animations();
    match app.window().take_snapshot() {
        Ok(buffer) => {
            let width = buffer.width();
            let height = buffer.height();
            // Fix: force alpha to 255 — Slint software renderer produces alpha=0
            let mut pixels = buffer.as_bytes().to_vec();
            for chunk in pixels.chunks_exact_mut(4) {
                chunk[3] = 255;
            }
            let path = screenshots_dir().join(format!("{}.png", name));
            let img = image::RgbaImage::from_raw(width, height, pixels)
                .expect("valid RGBA buffer");
            img.save(&path).expect("save PNG");
            eprintln!("  [SCREENSHOT] {:?} ({}x{})", path, width, height);
        }
        Err(e) => {
            eprintln!("  [WARNING] take_snapshot() failed for '{}': {}", name, e);
        }
    }
}

/// Main E2E visual test — runs all checks with a single App instance.
/// Each check navigates to a tab, verifies state, and captures a screenshot.
#[test]
fn e2e_all_surfaces() {
    init_test_platform();
    set_window_size(1200, 800);
    let app = App::new().expect("create App");
    app.show().expect("show App");

    let mut passed = 0u32;
    let mut failed = 0u32;

    macro_rules! check {
        ($name:expr, $body:block) => {{
            eprint!("  CHECK: {} ... ", $name);
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
                Ok(_) => { eprintln!("OK"); passed += 1; }
                Err(e) => {
                    let msg = e.downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| e.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    eprintln!("FAILED: {}", msg);
                    failed += 1;
                }
            }
        }};
    }

    eprintln!("\n=== Citrate GUI E2E Visual Tests ===\n");

    // ── 01. Dashboard (default tab) ──
    check!("dashboard_default", {
        assert_eq!(app.get_active_tab().to_string(), "dashboard");
        save_snapshot(&app, "01_dashboard_default");
    });

    // ── 02. Wallet tab ──
    check!("wallet_tab", {
        app.set_active_tab("wallet".into());
        assert_eq!(app.get_active_tab().to_string(), "wallet");
        let addr = app.get_wallet_selected_address().to_string();
        assert!(addr.is_empty() || addr.starts_with("0x"));
        save_snapshot(&app, "02_wallet_tab");
    });

    // ── 03. Chat tab ──
    check!("chat_tab", {
        app.set_active_tab("chat".into());
        assert_eq!(app.get_active_tab().to_string(), "chat");
        assert!(!app.get_chat_thinking());
        assert!(app.get_chat_error().to_string().is_empty());
        save_snapshot(&app, "03_chat_tab");
    });

    // ── 04. Chat tool approval card ──
    check!("chat_tool_approval", {
        app.set_active_tab("chat".into());
        app.set_chat_tool_pending(true);
        app.set_chat_tool_name("send_tx".into());
        app.set_chat_tool_description("Send 5 SALT to 0xb6E9...".into());
        app.set_chat_tool_risk_level("high".into());
        app.set_chat_tool_target("Address: 0xb6E9A558a4f9DC9E3F667a3B446a48bddF671126".into());
        app.set_chat_tool_scope("Send 5 SALT".into());
        assert!(app.get_chat_tool_pending());
        assert_eq!(app.get_chat_tool_risk_level().to_string(), "high");
        save_snapshot(&app, "04_chat_tool_approval");
        // Reset
        app.set_chat_tool_pending(false);
    });

    // ── 05. Chat tool disclosure ──
    check!("chat_tool_disclosure", {
        app.set_active_tab("chat".into());
        app.set_chat_tool_disclosure("Executed send_tx — Transaction sent. Hash: 0xabc123".into());
        assert!(!app.get_chat_tool_disclosure().to_string().is_empty());
        save_snapshot(&app, "05_chat_tool_disclosure");
        app.set_chat_tool_disclosure("".into());
    });

    // ── 06. Chat privacy local ──
    check!("chat_privacy_local", {
        app.set_active_tab("chat".into());
        app.set_chat_backend_type("local".into());
        assert_eq!(app.get_chat_backend_type().to_string(), "local");
        save_snapshot(&app, "06_chat_privacy_local");
    });

    // ── 07. Chat privacy API ──
    check!("chat_privacy_api", {
        app.set_chat_backend_type("api".into());
        assert_eq!(app.get_chat_backend_type().to_string(), "api");
        save_snapshot(&app, "07_chat_privacy_api");
        app.set_chat_backend_type("local".into());
    });

    // ── 08. Models tab (empty) ──
    check!("models_tab_empty", {
        app.set_active_tab("models".into());
        assert_eq!(app.get_active_tab().to_string(), "models");
        save_snapshot(&app, "08_models_tab_empty");
    });

    // ── 09. Models tab (with model) ──
    check!("models_tab_with_model", {
        app.set_chat_model_loaded(true);
        app.set_chat_model_name("qwen2.5:7b".into());
        save_snapshot(&app, "09_models_tab_with_model");
    });

    // (Contracts tab snapshots #10–#14 retired in P960-H along with
    // the Contracts surface itself.)

    // ── 15. Operations tab (default) ──
    check!("operations_default", {
        app.set_active_tab("operations".into());
        assert_eq!(app.get_active_tab().to_string(), "operations");
        assert_eq!(app.get_ops_pending_count(), 0);
        save_snapshot(&app, "15_operations_default");
    });

    // ── 16. Operations tab (active) ──
    check!("operations_active", {
        app.set_ops_pending_count(2);
        app.set_ops_trail_count(15);
        app.set_ops_active_sessions(1);
        app.set_ops_grant_scope("guided".into());
        app.set_ops_logseq_status("online".into());
        app.set_ops_logseq_path("~/logseq-graph".into());
        app.set_ops_hermes_status("offline".into());
        save_snapshot(&app, "16_operations_active");
        // Reset
        app.set_ops_pending_count(0);
        app.set_ops_trail_count(0);
    });

    // ── 17. Compute tab ──
    check!("compute_tab", {
        app.set_active_tab("compute".into());
        save_snapshot(&app, "17_compute_tab");
    });

    // ── 18. Learning tab ──
    check!("learning_tab", {
        app.set_active_tab("learning".into());
        save_snapshot(&app, "18_learning_tab");
    });

    // ── 19. Settings tab ──
    check!("settings_tab", {
        app.set_active_tab("settings".into());
        save_snapshot(&app, "19_settings_tab");
    });

    // ── 20. DAG Explorer tab ──
    check!("dag_explorer", {
        app.set_active_tab("dag".into());
        save_snapshot(&app, "20_dag_explorer");
    });

    // ── 21. Storage tab ──
    check!("storage_tab", {
        app.set_active_tab("storage".into());
        save_snapshot(&app, "21_storage_tab");
    });

    // ── 22. Dashboard with node running ──
    check!("dashboard_node_running", {
        app.set_active_tab("dashboard".into());
        app.set_node_running(true);
        app.set_block_height(7586);
        app.set_peer_count(4);
        assert!(app.get_node_running());
        assert_eq!(app.get_block_height(), 7586);
        save_snapshot(&app, "22_dashboard_node_running");
    });

    // ── 23. Wallet with balance ──
    check!("wallet_with_balance", {
        app.set_active_tab("wallet".into());
        app.set_wallet_selected_address("0xacEAA7d00C024d32e6E0A07094ceB1a7706786D1".into());
        app.set_wallet_balance("449,800,000 SALT".into());
        assert_eq!(app.get_wallet_balance().to_string(), "449,800,000 SALT");
        save_snapshot(&app, "23_wallet_with_balance");
    });

    // ── 24. Environment switch ──
    check!("environment_switch", {
        app.set_environment("Testnet".into());
        assert_eq!(app.get_environment().to_string(), "Testnet");
        app.set_environment("Devnet".into());
        assert_eq!(app.get_environment().to_string(), "Devnet");
        app.set_environment("Testnet".into());
    });

    // ── 25-27. Resolution tests ──
    check!("resolution_800x600", {
        set_window_size(800, 600);
        app.set_active_tab("dashboard".into());
        save_snapshot(&app, "25_resolution_800x600");
    });

    check!("resolution_1200x800", {
        set_window_size(1200, 800);
        save_snapshot(&app, "26_resolution_1200x800");
    });

    check!("resolution_1440x960", {
        set_window_size(1440, 960);
        save_snapshot(&app, "27_resolution_1440x960");
    });

    // ── Summary ──
    eprintln!("\n=== Results: {} passed, {} failed out of {} total ===\n",
        passed, failed, passed + failed);

    // List all screenshots
    let dir = screenshots_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        files.sort_by_key(|e| e.file_name());
        eprintln!("Screenshots ({}):", files.len());
        for entry in &files {
            if let Ok(meta) = entry.metadata() {
                eprintln!("  {} ({} KB)", entry.file_name().to_string_lossy(), meta.len() / 1024);
            }
        }
    }

    assert_eq!(failed, 0, "{} checks failed — see output above", failed);
}
