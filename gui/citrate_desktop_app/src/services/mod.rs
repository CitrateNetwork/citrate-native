//! Typed service interfaces for the desktop application.
//!
//! Each service wraps domain crate functionality and exposes
//! typed methods that the UI layer consumes directly.
//! No IPC, no JSON, no string payloads.

pub mod node_service;
pub mod wallet_service;
// (editor_service, file_explorer_service, compiler_service,
// git_service, terminal_service retired in P960-H — see git
// history for the implementations if a future panel needs them.)
pub mod block_service;
pub mod chat_service;
pub mod compute_service;
pub mod edu;
pub mod learning_service;
pub mod mcp_host;
pub mod model_service;
pub mod relay_service;
// EW-S1 WP-8: ERC-4337/Kernel helpers for the Citrate identity link.
pub mod citrate_aa;
pub mod citrate_link_service;

pub use block_service::BlockService;
pub use chat_service::ChatService;
pub use compute_service::ComputeService;
pub use learning_service::LearningService;
pub use model_service::ModelService;
pub use node_service::NodeService;
pub use wallet_service::WalletService;
