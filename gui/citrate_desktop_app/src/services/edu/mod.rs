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

#[cfg(test)]
mod tests {
    /// The education contract addresses must equal the vendored canonical book
    /// (`gui/citrate_native/src/generated/addresses.json`, refreshed by
    /// `scripts/sync-addresses.sh`), so a reroll cannot leave them stale.
    #[test]
    fn edu_addresses_match_the_vendored_book() {
        let book: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../citrate_native/src/generated/addresses.json"
        ))
        .expect("vendored book parses");
        assert_eq!(book["chainId"].as_u64(), Some(40204));
        let contracts = &book["contracts"];
        for (name, ours) in [
            (
                "InstitutionalVault",
                super::institutional_service::VAULT_ADDRESS,
            ),
            (
                "CashoutRequest",
                super::institutional_service::CASHOUT_ADDRESS,
            ),
            (
                "ClassroomClusterV1",
                super::classroom_service::CLUSTER_ADDRESS,
            ),
            ("BudgetAllocation", super::budget_service::BUDGET_ADDRESS),
            ("EduForwarder", super::forwarder_service::FORWARDER_ADDRESS),
        ] {
            let canon = contracts[name].as_str().unwrap_or_default().to_lowercase();
            assert!(!canon.is_empty(), "{name} missing from the vendored book");
            assert_eq!(canon, ours, "{name} drifted from the vendored book");
        }
    }
}
