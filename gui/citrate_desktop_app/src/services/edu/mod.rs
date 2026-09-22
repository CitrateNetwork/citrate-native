//! Education stack backend services.
//!
//! Provides typed Rust services for querying the 5 institutional education
//! contracts deployed on chain 40204:
//!
//! - InstitutionalVault (multi-sig treasury)
//! - ClassroomClusterV1 (RBAC + classroom management)
//! - BudgetAllocation + CashoutRequest (financial lifecycle)
//! - Forwarder (EIP-2771 meta-tx relay)
//!
//! Each service follows the crate's trait-backend pattern:
//!   trait XxxBackend → struct RpcXxxBackend (real on-chain queries)

mod abi;
pub mod budget_service;
pub mod classroom_service;
pub mod forwarder_service;
pub mod institutional_service;
