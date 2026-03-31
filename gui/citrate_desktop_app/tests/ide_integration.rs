//! Integration tests — cross-service interactions.
//!
//! Tests the full IDE workflow: open project, browse files,
//! edit code, compile, commit, and verify state consistency
//! across all services.

use citrate_desktop_app::services::editor_service::EditorService;
use citrate_desktop_app::services::file_explorer_service::{
    FileExplorerService, RealFileExplorerBackend,
};
use citrate_desktop_app::services::git_service::GitService;
use citrate_desktop_app::services::node_service::{NodeBackend, NodeService};
use citrate_desktop_app::event_bus::{AppEvent, EventBus};
use citrate_desktop_app::error::AppError;
use citrate_desktop_app::{AppConfig, AppCore};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Test node backend — no RocksDB, no P2P, no port binding
struct TestNodeBackend;

#[async_trait::async_trait]
impl NodeBackend for TestNodeBackend {
    async fn start_node(&self, _: u64, _: &str) -> Result<(), AppError> { Ok(()) }
    async fn stop_node(&self) -> Result<(), AppError> { Ok(()) }
    async fn get_block_height(&self) -> u64 { 0 }
    async fn get_peer_count(&self) -> u32 { 0 }
    async fn get_mempool_size(&self) -> usize { 0 }
    async fn get_balance(&self, _: &[u8; 20]) -> String { "0".to_string() }
}

/// Create AppCore with test node backend (R-02 fix: hermetic, no real data dir)
fn test_app_core() -> AppCore {
    let config = Arc::new(RwLock::new(AppConfig::default()));
    let events = Arc::new(EventBus::new());
    let node = Arc::new(NodeService::with_backend(config.clone(), events.clone(), Arc::new(TestNodeBackend)));
    let wallet = Arc::new(citrate_desktop_app::services::WalletService::new(events.clone()));
    let editor = Arc::new(citrate_desktop_app::services::EditorService::new(events.clone()));
    let file_explorer = Arc::new(citrate_desktop_app::services::FileExplorerService::new(events.clone()));
    let git = Arc::new(citrate_desktop_app::services::GitService::new(events.clone()));
    let compiler = Arc::new(citrate_desktop_app::services::CompilerService::new(events.clone()));
    let terminal = Arc::new(citrate_desktop_app::services::TerminalService::new(events.clone()));
    let rpc_url = format!("http://127.0.0.1:{}", 18545);
    let chat = Arc::new(citrate_desktop_app::services::ChatService::new(events.clone(), &rpc_url));
    let models = Arc::new(citrate_desktop_app::services::ModelService::new(events.clone(), &rpc_url));
    let blocks = Arc::new(citrate_desktop_app::services::BlockService::new(events.clone(), &rpc_url));
    let learning = Arc::new(citrate_desktop_app::services::LearningService::new(events.clone(), &rpc_url));
    let compute = Arc::new(citrate_desktop_app::services::ComputeService::new(events.clone(), &rpc_url));
    AppCore { node, wallet, editor, file_explorer, git, compiler, terminal, chat, models, blocks, learning, compute, events, config }
}

fn test_dir(_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("citrate_integ_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

// =========================================================================
// EDITOR + FILE EXPLORER INTEGRATION
// =========================================================================

#[tokio::test]
async fn test_open_file_from_explorer() {
    let dir = test_dir("editor_explorer");
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").expect("write");

    let events = Arc::new(EventBus::new());
    let editor = EditorService::new(events.clone());
    let explorer = FileExplorerService::with_backend(events, Arc::new(RealFileExplorerBackend));

    explorer.set_root(&dir).await.expect("set root");
    let tree = explorer.get_tree().await;
    assert!(!tree.is_empty());

    // Find the file in the tree
    let file_node = tree.iter().find(|n| n.name == "main.rs").expect("file in tree");

    // Open it in the editor
    let buf_id = editor.open_file(Path::new(&file_node.path)).await.expect("open file");
    let content = editor.get_content(&buf_id).await.expect("get content");
    assert_eq!(content, "fn main() {}\n");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_save_reflects_on_disk() {
    let dir = test_dir("save_disk");
    let file_path = dir.join("test.sol");
    std::fs::write(&file_path, "// original").expect("write");

    let events = Arc::new(EventBus::new());
    let editor = EditorService::new(events);

    let buf_id = editor.open_file(&file_path).await.expect("open");
    editor.insert_text(&buf_id, 11, "\n// added line").await.expect("insert");
    editor.save_buffer(&buf_id).await.expect("save");

    // Verify on disk
    let disk_content = std::fs::read_to_string(&file_path).expect("read from disk");
    assert!(disk_content.contains("added line"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_create_file_then_open_in_editor() {
    let dir = test_dir("create_open");
    let events = Arc::new(EventBus::new());
    let explorer = FileExplorerService::with_backend(events.clone(), Arc::new(RealFileExplorerBackend));
    let editor = EditorService::new(events);

    explorer.set_root(&dir).await.expect("set root");

    let new_file = dir.join("NewContract.sol");
    explorer.create_file(&new_file, "pragma solidity ^0.8.0;").await.expect("create file");

    let buf_id = editor.open_file(&new_file).await.expect("open new file");
    let content = editor.get_content(&buf_id).await.expect("content");
    assert_eq!(content, "pragma solidity ^0.8.0;");

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// EDITOR + GIT INTEGRATION
// =========================================================================

#[tokio::test]
async fn test_edit_file_shows_in_git_status() {
    let dir = test_dir("edit_git");

    // Init git repo
    let repo = git2::Repository::init(&dir).expect("git init");
    {
        let mut config = repo.config().expect("config");
        config.set_str("user.name", "Test").expect("set name");
        config.set_str("user.email", "test@test.com").expect("set email");
    }

    // Create and commit a file
    let file_path = dir.join("Token.sol");
    std::fs::write(&file_path, "// v1").expect("write");
    {
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("Token.sol")).expect("add");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = repo.signature().expect("sig");
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[]).expect("commit");
    }

    // Now modify the file
    std::fs::write(&file_path, "// v2 modified").expect("modify");

    // Git should see it as modified
    let events = Arc::new(EventBus::new());
    let git = GitService::new(events.clone());
    git.open_repo(&dir).await.expect("open repo");

    let status = git.status().await;
    assert!(!status.is_empty(), "Modified file should appear in status");
    assert!(status.iter().any(|s| s.path.contains("Token.sol")));

    // Editor should also see the file
    let editor = EditorService::new(events);
    let buf_id = editor.open_file(&file_path).await.expect("open");
    let content = editor.get_content(&buf_id).await.expect("content");
    assert_eq!(content, "// v2 modified");

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// FULL WORKFLOW — open project, edit, save, compile-check, commit
// =========================================================================

#[tokio::test]
async fn test_full_ide_workflow() {
    let dir = test_dir("full_workflow");

    // Step 1: Init git repo
    let repo = git2::Repository::init(&dir).expect("git init");
    {
        let mut config = repo.config().expect("config");
        config.set_str("user.name", "Larry").expect("set name");
        config.set_str("user.email", "larry@citrate.ai").expect("set email");
        let sig = repo.signature().expect("sig");
        let mut index = repo.index().expect("index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_oid).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).expect("commit");
    }

    let events = Arc::new(EventBus::new());
    let explorer = FileExplorerService::with_backend(events.clone(), Arc::new(RealFileExplorerBackend));
    let editor = EditorService::new(events.clone());
    let git = GitService::new(events.clone());

    // Step 2: Set project root
    explorer.set_root(&dir).await.expect("set root");

    // Step 3: Create a new file
    let sol_path = dir.join("Token.sol");
    explorer.create_file(&sol_path, "pragma solidity ^0.8.0;\n").await.expect("create");

    // Step 4: Open in editor
    let buf_id = editor.open_file(&sol_path).await.expect("open");
    let info = editor.get_buffer_info(&buf_id).await.expect("info");
    assert_eq!(info.language, "Solidity");

    // Step 5: Edit the file
    let content_len = editor.get_content(&buf_id).await.expect("content").len();
    editor.insert_text(&buf_id, content_len, "\ncontract Token {\n    uint256 public supply;\n}\n")
        .await.expect("insert");
    assert!(editor.is_dirty(&buf_id).await.expect("dirty"));

    // Step 6: Save
    editor.save_buffer(&buf_id).await.expect("save");
    assert!(!editor.is_dirty(&buf_id).await.expect("dirty after save"));

    // Step 7: Verify file explorer sees it
    let tree = explorer.get_tree().await;
    assert!(tree.iter().any(|n| n.name == "Token.sol"), "File should be in tree");

    // Step 8: Git should see it as untracked
    git.open_repo(&dir).await.expect("open repo");
    let status = git.status().await;
    assert!(!status.is_empty(), "New file should be in git status");

    // Step 9: Stage and commit
    git.stage(&[Path::new("Token.sol")]).await.expect("stage");
    let hash = git.commit("Add Token contract").await.expect("commit");
    assert!(!hash.is_empty());

    // Step 10: Verify commit in log
    let log = git.log(1).await.expect("log");
    assert!(!log.is_empty());
    assert!(log.iter().any(|c| c.message.contains("Token")));

    // Step 11: Verify clean status
    let status = git.status().await;
    assert!(status.is_empty(), "Status should be clean after commit");

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// EVENT FLOW — verify events propagate across services
// =========================================================================

#[tokio::test]
async fn test_editor_events_flow() {
    let dir = test_dir("editor_events");
    std::fs::write(dir.join("test.rs"), "hello").expect("write");

    let events = Arc::new(EventBus::new());
    let mut rx = events.subscribe();
    let editor = EditorService::new(events);

    let id = editor.open_file(&dir.join("test.rs")).await.expect("open");
    let event = rx.recv().await.expect("event received");
    assert!(matches!(event, AppEvent::EditorBufferChanged { .. }));

    editor.insert_text(&id, 5, " world").await.expect("insert");
    let event = rx.recv().await.expect("event received");
    assert!(matches!(event, AppEvent::EditorBufferChanged { .. }));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_file_explorer_events_flow() {
    let events = Arc::new(EventBus::new());
    let mut rx = events.subscribe();
    let explorer = FileExplorerService::with_backend(events, Arc::new(RealFileExplorerBackend));

    let dir = test_dir("events_flow");
    explorer.set_root(&dir).await.expect("set root");

    let event = rx.recv().await.expect("event received");
    assert!(matches!(event, AppEvent::FileTreeChanged));

    std::fs::remove_dir_all(&dir).ok();
}

// =========================================================================
// APP CORE INTEGRATION — all services accessible
// =========================================================================

#[tokio::test]
async fn test_app_core_has_all_services() {
    let core = test_app_core();

    // All services should be accessible
    assert!(core.editor.list_open_buffers().await.is_empty());
    assert!(core.file_explorer.get_tree().await.is_empty());
    assert!(core.terminal.list_sessions().await.is_empty());
    let _ = core.git.current_branch().await; // may error if no repo
    let _ = core.compiler.is_compiling().await;

    // Node and wallet
    let status = core.node.get_status().await;
    assert_eq!(status.chain_id, 40204);
    assert!(core.wallet.is_first_run().await);
}

#[tokio::test]
async fn test_app_core_services_share_event_bus() {
    let core = test_app_core();
    let mut rx = core.events.subscribe();

    // Start node — should generate an event
    core.node.start().await.expect("start succeeded");
    let event = rx.recv().await.expect("event received");
    assert!(matches!(event, AppEvent::NodeStatusChanged { running: true, .. }));
}

// =========================================================================
// CONCURRENT CROSS-SERVICE OPERATIONS
// =========================================================================

#[tokio::test]
async fn test_concurrent_editor_and_explorer() {
    let dir = test_dir("concurrent");
    for i in 0..5 {
        std::fs::write(dir.join(format!("file_{}.rs", i)), format!("// file {}", i)).expect("write");
    }

    let events = Arc::new(EventBus::new());
    let editor = Arc::new(EditorService::new(events.clone()));
    let explorer = Arc::new(FileExplorerService::with_backend(events, Arc::new(RealFileExplorerBackend)));

    explorer.set_root(&dir).await.expect("set root");

    // Open all files in parallel
    let mut handles = vec![];
    for i in 0..5 {
        let editor = editor.clone();
        let path = dir.join(format!("file_{}.rs", i));
        handles.push(tokio::spawn(async move {
            editor.open_file(&path).await.expect("open file")
        }));
    }

    for h in handles {
        h.await.expect("join succeeded");
    }

    let buffers = editor.list_open_buffers().await;
    assert_eq!(buffers.len(), 5, "All 5 files should be open");

    std::fs::remove_dir_all(&dir).ok();
}
