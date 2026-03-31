//! Typed service interfaces for the desktop application.
//!
//! Each service wraps domain crate functionality and exposes
//! typed methods that the UI layer consumes directly.
//! No IPC, no JSON, no string payloads.

pub mod node_service;
pub mod wallet_service;
pub mod editor_service;
pub mod file_explorer_service;
pub mod compiler_service;
pub mod git_service;
pub mod terminal_service;
pub mod chat_service;
pub mod model_service;
pub mod block_service;
pub mod learning_service;
pub mod compute_service;

pub use node_service::NodeService;
pub use wallet_service::WalletService;
pub use editor_service::EditorService;
pub use file_explorer_service::FileExplorerService;
pub use compiler_service::CompilerService;
pub use git_service::GitService;
pub use terminal_service::TerminalService;
pub use chat_service::ChatService;
pub use model_service::ModelService;
pub use block_service::BlockService;
pub use learning_service::LearningService;
pub use compute_service::ComputeService;
