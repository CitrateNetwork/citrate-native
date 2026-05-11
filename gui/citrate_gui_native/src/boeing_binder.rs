//! BFR-INT-1 — Boeing FedRAMP panel wiring for the citrate-gui-native shell.
//!
//! Holds the 16+ `Live*Bindings` instances that each adapter crate
//! (`citrate-boeing-{overview,provenance,suppliers,fl,models_compute,
//! apps_contracts,assistant,governance,ontology}`) consumes when it
//! assembles its `*PanelData` view model. Addresses are sourced from
//! [`.agentile/CONFIG.md`] — they are the live deployments on chain
//! 40204 (testnet beta) under the BFR program stages 1, 7–15.
//!
//! Construction is fail-fast: if any address is malformed or the RPC
//! URL is empty, `BoeingBindings::new` returns an error and the caller
//! decides whether to abort the GUI bring-up or to fall back to
//! mock/empty-state.

use citrate_rbac_bindings::{
    apps_contracts::{AppRegistryBindings, CrossOrgIndexBindings},
    apps_contracts_live::{LiveAppRegistryBindings, LiveCrossOrgIndexBindings},
    assistant::{AssistantBindings, AuditBundleBindings},
    assistant_live::{LiveAssistantBindings, LiveAuditBundleBindings},
    compliance_live::LiveTripwireBindings,
    fl::{FlScopeBindings, LearningPoolBindings},
    fl_live::{LiveFlScopeBindings, LiveLearningPoolBindings},
    governance::{ComplianceBindings, RoleGrantTenantIndexBindings},
    governance_live::{LiveComplianceBindings, LiveRoleGrantTenantIndexBindings},
    inter_org_live::LiveCrossOrgEnvelopeBindings,
    live::{LiveRbacBindings, RbacAddresses},
    models_compute::{ComputeMarketplaceBindings, ModelRegistryBindings, TeeAttestationBindings},
    models_compute_live::{
        LiveComputeMarketplaceBindings, LiveModelRegistryBindings, LiveTeeAttestationBindings,
    },
    ontology::{EntityRegistryBindings, TenantChildrenBindings},
    ontology_live::{LiveEntityRegistryBindings, LiveTenantChildrenBindings},
    procurement_live::LiveTinaWorkpaperBindings,
    provenance::ProvenanceBindings,
    provenance_live::LiveProvenanceBindings,
    release_live::LiveReleaseManifestBindings,
    sponsor_live::LiveSponsorEvidenceBindings,
    suppliers::{MoqBindings, SupplierBindings},
    suppliers_live::{LiveMoqBindings, LiveSupplierBindings},
    RbacBindings,
};
use std::sync::Arc;
use thiserror::Error;

/// Errors raised during `BoeingBindings` construction.
#[derive(Debug, Error)]
pub enum BoeingBindingsError {
    /// One of the hex-encoded addresses in the chain-40204 manifest
    /// failed to parse to a 20-byte array.
    #[error("malformed contract address `{0}`: {1}")]
    BadAddress(&'static str, String),
    /// One of the bindings' constructors rejected its input (usually
    /// an empty RPC URL).
    #[error("LiveBindings init failed for {0}: {1}")]
    LiveInit(&'static str, String),
}

/// Decode an `0x`-prefixed hex address into a 20-byte array.
fn decode_addr(name: &'static str, hex: &str) -> Result<[u8; 20], BoeingBindingsError> {
    let stripped = hex.trim_start_matches("0x");
    let bytes =
        hex::decode(stripped).map_err(|e| BoeingBindingsError::BadAddress(name, e.to_string()))?;
    if bytes.len() != 20 {
        return Err(BoeingBindingsError::BadAddress(
            name,
            format!("expected 20 bytes, got {}", bytes.len()),
        ));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Chain-40204 contract addresses for the BFR program. Source:
/// [`.agentile/CONFIG.md`] §Stage-1 + §Stages 7–15. The constants are
/// kept in one place so a future stage-N redeploy needs exactly one
/// edit.
mod addr {
    // Stage-1: RBAC foundation (BFR-02)
    pub const TENANT_HIERARCHY: &str = "0x3FF095445b382075971fD5D3e05FD8bB3FF8006C";
    pub const CLASSIFICATION_REGISTRY: &str = "0x933E6f4d28E3ebeD462227522d839A77C85B4c06";
    pub const ROLE_ESCALATION: &str = "0x3130B9494Dc9c9253078176917cF4CDdcEf48337";
    pub const MULTI_SIG_ENVELOPE: &str = "0x05825775315f3d074db9F948713D05059e12a8Fd";
    pub const AGENT_DECISION_REGISTRY_V2: &str = "0x4a86659BDab24dc444C72fbbaD4cd83491820E40";
    pub const CONTRADICTION_LEDGER: &str = "0x25051e90A110fbE4569f124274ce387eB033bC9c";
    // BFR-05 Provenance
    pub const PART_PROVENANCE_REGISTRY: &str = "0xF0dCa50F418acFb8917D71d8bB65393308629381";
    // BFR-06 Suppliers + MOQ
    pub const SUPPLIER_REGISTRY: &str = "0xdE991179021A208cF7E6caeBF3a07c229aEd3D0F";
    pub const MOQ_REGISTRY: &str = "0x575d0d85e272eca8784a4D11F4713C698082c807";
    // BFR-07 Federated Learning index + LearningPool
    pub const BOEING_FL_SCOPE_INDEX: &str = "0x26BAD758EAC1bac02457F8e4544269b8B52BC5d7";
    pub const LEARNING_POOL: &str = "0x1f73BB479f397A34B5E3145e51d25bC5007273Bf";
    // BFR-08 Models / Compute (refreshed 2026-05-11)
    pub const MODEL_REGISTRY: &str = "0x077fbc3338a9e6bad90a3a041e6b7425689754ef";
    pub const COMPUTE_MARKETPLACE: &str = "0x18D3e03eb3364f63Db8E4F6BBD078aD8098c2C2B";
    pub const TEE_ATTESTATION_REGISTRY: &str = "0xcB5fcAD35f892E7E1dA4bB4D17a48DD9e056583e";
    // Stage-7: Apps & Contracts (BFR-09)
    pub const APP_REGISTRY: &str = "0xdAff2B9DC254B6CB3040F8f14304d30E136fa136";
    pub const CROSS_ORG_INDEX: &str = "0xb87a4F754CA316D2416553d04F4edEd26424B536";
    // Stage-8: Audit Bundle (BFR-10)
    pub const AUDIT_BUNDLE_REGISTRY: &str = "0xcedfd8d76e0d755e9bc93a06fa339c397b70f38d";
    // Stage-9: Compliance + Role-grant index (BFR-11)
    pub const BOEING_COMPLIANCE_REGISTRY: &str = "0x8dbbbc46d840f40205b48d76aa9fc5063b7d55d8";
    pub const ROLE_GRANT_TENANT_INDEX: &str = "0x1f7a33edf743349d1cb86ea3a219f6beb3e647c8";
    // Stage-10: Entity Registry (BFR-12)
    pub const ENTITY_REGISTRY: &str = "0x16041ddf6cdb49d3a2d46da23b5c5820bbd92a66";
    // Stage-11: TINA Workpaper (BFR-13)
    pub const TINA_WORKPAPER_REGISTRY: &str = "0xc503fdb502d40c7317afe1285209e0508a5cc63f";
    // Stage-12: Cross-org envelope (BFR-14)
    pub const CROSS_ORG_ENVELOPE: &str = "0x9871e73a189885f87c9c9ec41a6b0c98175c99f8";
    // Stage-13: Tripwire (BFR-15)
    pub const TRIPWIRE_REGISTRY: &str = "0xa37091480df4d380d1d2eee58ead6b3a014fe70f";
    // Stage-14: Sponsor evidence (BFR-16)
    pub const SPONSOR_EVIDENCE_REGISTRY: &str = "0xf9d198e1280b0d1952e4ff63bd87bcae44b4f82e";
    // Stage-15: Release manifest (BFR-17 — FINAL)
    pub const RELEASE_MANIFEST_REGISTRY: &str = "0xb92f631bb7f16763c00e52d95f5627806a1284f6";
}

/// Aggregate of all live trait objects the Boeing panel adapters need.
///
/// Each adapter `fetch_*` / `assemble_*` function takes one or more of
/// these as `&dyn ...Bindings`. Holding them as `Arc<dyn ...>` lets
/// callbacks `clone` the handle into async tasks without unsafe.
pub struct BoeingBindings {
    pub rbac: Arc<dyn RbacBindings>,
    pub provenance: Arc<dyn ProvenanceBindings>,
    pub suppliers: Arc<dyn SupplierBindings>,
    pub moqs: Arc<dyn MoqBindings>,
    pub fl_scope: Arc<dyn FlScopeBindings>,
    pub learning_pool: Arc<dyn LearningPoolBindings>,
    pub model_registry: Arc<dyn ModelRegistryBindings>,
    pub compute_marketplace: Arc<dyn ComputeMarketplaceBindings>,
    pub tee_attestation: Arc<dyn TeeAttestationBindings>,
    pub app_registry: Arc<dyn AppRegistryBindings>,
    pub cross_org_index: Arc<dyn CrossOrgIndexBindings>,
    pub assistant: Arc<dyn AssistantBindings>,
    pub audit_bundle: Arc<dyn AuditBundleBindings>,
    pub compliance: Arc<dyn ComplianceBindings>,
    pub role_grant_tenant_index: Arc<dyn RoleGrantTenantIndexBindings>,
    pub tenant_children: Arc<dyn TenantChildrenBindings>,
    pub entity_registry: Arc<dyn EntityRegistryBindings>,
    /// Stage-12 inter-org envelope adapter. Held for callbacks that
    /// drive `CrossOrgEnvelope.draft(...)` flows; the read-only
    /// panels above route through `app_registry` / `cross_org_index`.
    pub cross_org_envelope: Arc<LiveCrossOrgEnvelopeBindings>,
    /// Stage-11 workpaper adapter, exposed for the procurement
    /// drawer in the suppliers panel.
    pub tina_workpaper: Arc<LiveTinaWorkpaperBindings>,
    /// Stage-13 tripwire adapter, exposed for the governance panel.
    pub tripwire: Arc<LiveTripwireBindings>,
    /// Stage-14 sponsor evidence adapter, exposed for the operations
    /// panel's "Anchor evidence bundle" flow.
    pub sponsor_evidence: Arc<LiveSponsorEvidenceBindings>,
    /// Stage-15 release manifest adapter, exposed for the operations
    /// panel's release-ladder UI.
    pub release_manifest: Arc<LiveReleaseManifestBindings>,
}

impl BoeingBindings {
    /// Construct all live bindings against `rpc_url`. Returns an error
    /// if any address in the BFR program manifest is malformed or any
    /// adapter rejects construction.
    pub fn new(rpc_url: impl Into<String>) -> Result<Self, BoeingBindingsError> {
        let rpc_url: String = rpc_url.into();
        if rpc_url.is_empty() {
            return Err(BoeingBindingsError::LiveInit(
                "BoeingBindings",
                "rpc_url empty".into(),
            ));
        }

        // Decode all stage-N addresses up front (fail fast).
        let rbac_addrs = RbacAddresses {
            tenant_hierarchy: decode_addr("TenantHierarchy", addr::TENANT_HIERARCHY)?,
            role_escalation: decode_addr("RoleEscalation", addr::ROLE_ESCALATION)?,
            classification_registry: decode_addr(
                "ClassificationRegistry",
                addr::CLASSIFICATION_REGISTRY,
            )?,
            multi_sig_envelope: decode_addr("MultiSigEnvelope", addr::MULTI_SIG_ENVELOPE)?,
            agent_decision_registry_v2: decode_addr(
                "AgentDecisionRegistryV2",
                addr::AGENT_DECISION_REGISTRY_V2,
            )?,
            contradiction_ledger: decode_addr("ContradictionLedger", addr::CONTRADICTION_LEDGER)?,
        };

        let part_prov = decode_addr("PartProvenanceRegistry", addr::PART_PROVENANCE_REGISTRY)?;
        let supplier = decode_addr("SupplierRegistry", addr::SUPPLIER_REGISTRY)?;
        let moq = decode_addr("MoqRegistry", addr::MOQ_REGISTRY)?;
        let fl_scope = decode_addr("BoeingFLScopeIndex", addr::BOEING_FL_SCOPE_INDEX)?;
        let learning_pool = decode_addr("LearningPool", addr::LEARNING_POOL)?;
        let model_registry = decode_addr("ModelRegistry", addr::MODEL_REGISTRY)?;
        let compute_marketplace = decode_addr("ComputeMarketplace", addr::COMPUTE_MARKETPLACE)?;
        let tee_attestation =
            decode_addr("TEEAttestationRegistry", addr::TEE_ATTESTATION_REGISTRY)?;
        let app_registry = decode_addr("AppRegistry", addr::APP_REGISTRY)?;
        let cross_org_index = decode_addr("CrossOrgIndex", addr::CROSS_ORG_INDEX)?;
        let audit_bundle = decode_addr("AuditBundleRegistry", addr::AUDIT_BUNDLE_REGISTRY)?;
        let compliance_reg =
            decode_addr("BoeingComplianceRegistry", addr::BOEING_COMPLIANCE_REGISTRY)?;
        let role_grant_idx = decode_addr("RoleGrantTenantIndex", addr::ROLE_GRANT_TENANT_INDEX)?;
        let entity_reg = decode_addr("EntityRegistry", addr::ENTITY_REGISTRY)?;
        let workpaper = decode_addr("TinaWorkpaperRegistry", addr::TINA_WORKPAPER_REGISTRY)?;
        let cross_org_env = decode_addr("CrossOrgEnvelope", addr::CROSS_ORG_ENVELOPE)?;
        let tripwire = decode_addr("TripwireRegistry", addr::TRIPWIRE_REGISTRY)?;
        let sponsor = decode_addr("SponsorEvidenceRegistry", addr::SPONSOR_EVIDENCE_REGISTRY)?;
        let release = decode_addr("ReleaseManifestRegistry", addr::RELEASE_MANIFEST_REGISTRY)?;

        // Construct each live binding. Helper to compress error wrap.
        let init = |name: &'static str, s: String| BoeingBindingsError::LiveInit(name, s);

        let rbac =
            LiveRbacBindings::new(rpc_url.clone(), rbac_addrs).map_err(|e| init("Rbac", format!("{e:?}")))?;
        let provenance = LiveProvenanceBindings::new(rpc_url.clone(), part_prov)
            .map_err(|e| init("Provenance", format!("{e:?}")))?;
        let suppliers = LiveSupplierBindings::new(rpc_url.clone(), supplier)
            .map_err(|e| init("Supplier", format!("{e:?}")))?;
        let moqs = LiveMoqBindings::new(rpc_url.clone(), moq)
            .map_err(|e| init("Moq", format!("{e:?}")))?;
        let fl_scope = LiveFlScopeBindings::new(rpc_url.clone(), fl_scope)
            .map_err(|e| init("FlScope", format!("{e:?}")))?;
        let learning_pool = LiveLearningPoolBindings::new(rpc_url.clone(), learning_pool)
            .map_err(|e| init("LearningPool", format!("{e:?}")))?;
        let model_registry = LiveModelRegistryBindings::new(rpc_url.clone(), model_registry)
            .map_err(|e| init("ModelRegistry", format!("{e:?}")))?;
        let compute_marketplace =
            LiveComputeMarketplaceBindings::new(rpc_url.clone(), compute_marketplace)
                .map_err(|e| init("ComputeMarketplace", format!("{e:?}")))?;
        let tee_attestation = LiveTeeAttestationBindings::new(rpc_url.clone(), tee_attestation)
            .map_err(|e| init("TeeAttestation", format!("{e:?}")))?;
        let app_reg = LiveAppRegistryBindings::new(rpc_url.clone(), app_registry)
            .map_err(|e| init("AppRegistry", format!("{e:?}")))?;
        let coi = LiveCrossOrgIndexBindings::new(rpc_url.clone(), cross_org_index)
            .map_err(|e| init("CrossOrgIndex", format!("{e:?}")))?;
        let assistant =
            LiveAssistantBindings::new(rpc_url.clone(), rbac_addrs.agent_decision_registry_v2)
                .map_err(|e| init("Assistant", format!("{e:?}")))?;
        let audit_bundle_b = LiveAuditBundleBindings::new(rpc_url.clone(), audit_bundle)
            .map_err(|e| init("AuditBundle", format!("{e:?}")))?;
        let compliance_b = LiveComplianceBindings::new(rpc_url.clone(), compliance_reg)
            .map_err(|e| init("Compliance", format!("{e:?}")))?;
        let role_grant = LiveRoleGrantTenantIndexBindings::new(rpc_url.clone(), role_grant_idx)
            .map_err(|e| init("RoleGrantTenantIndex", format!("{e:?}")))?;
        let tenant_children =
            LiveTenantChildrenBindings::new(rpc_url.clone(), rbac_addrs.tenant_hierarchy)
                .map_err(|e| init("TenantChildren", format!("{e:?}")))?;
        let entity = LiveEntityRegistryBindings::new(rpc_url.clone(), entity_reg)
            .map_err(|e| init("EntityRegistry", format!("{e:?}")))?;
        let cross_org_envelope = LiveCrossOrgEnvelopeBindings::new(rpc_url.clone(), cross_org_env)
            .map_err(|e| init("CrossOrgEnvelope", format!("{e:?}")))?;
        let tina = LiveTinaWorkpaperBindings::new(rpc_url.clone(), workpaper)
            .map_err(|e| init("TinaWorkpaper", format!("{e:?}")))?;
        let tripwire_b = LiveTripwireBindings::new(rpc_url.clone(), tripwire)
            .map_err(|e| init("Tripwire", format!("{e:?}")))?;
        let sponsor_b = LiveSponsorEvidenceBindings::new(rpc_url.clone(), sponsor)
            .map_err(|e| init("SponsorEvidence", format!("{e:?}")))?;
        let release_b = LiveReleaseManifestBindings::new(rpc_url, release)
            .map_err(|e| init("ReleaseManifest", format!("{e:?}")))?;

        Ok(Self {
            rbac: Arc::new(rbac),
            provenance: Arc::new(provenance),
            suppliers: Arc::new(suppliers),
            moqs: Arc::new(moqs),
            fl_scope: Arc::new(fl_scope),
            learning_pool: Arc::new(learning_pool),
            model_registry: Arc::new(model_registry),
            compute_marketplace: Arc::new(compute_marketplace),
            tee_attestation: Arc::new(tee_attestation),
            app_registry: Arc::new(app_reg),
            cross_org_index: Arc::new(coi),
            assistant: Arc::new(assistant),
            audit_bundle: Arc::new(audit_bundle_b),
            compliance: Arc::new(compliance_b),
            role_grant_tenant_index: Arc::new(role_grant),
            tenant_children: Arc::new(tenant_children),
            entity_registry: Arc::new(entity),
            cross_org_envelope: Arc::new(cross_org_envelope),
            tina_workpaper: Arc::new(tina),
            tripwire: Arc::new(tripwire_b),
            sponsor_evidence: Arc::new(sponsor_b),
            release_manifest: Arc::new(release_b),
        })
    }

    /// Default Boeing tenant root (chain 40204 testnet). Mirrors
    /// `.agentile/CONFIG.md` § Tenant roots. Used as the
    /// initial scope for panels that take a `scope: [u8; 32]`.
    pub fn boeing_tenant_root() -> [u8; 32] {
        // `keccak256("boeing-root")` — placeholder until BFR-INT-2
        // resolves the operator-set Boeing root from
        // `TenantHierarchy.initRoot`. Same constant the BFR-04 panels
        // used during visual proof regen so empty-state matches.
        use sha3::{Digest, Keccak256};
        let mut h = Keccak256::new();
        h.update(b"boeing-root");
        let out = h.finalize();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_addr_round_trips_known_address() {
        let bytes = decode_addr("test", addr::APP_REGISTRY).expect("addr parses");
        assert_eq!(bytes.len(), 20);
        // `0xdaff...` → first byte 0xda
        assert_eq!(bytes[0], 0xda);
    }

    #[test]
    fn decode_addr_rejects_short_input() {
        let r = decode_addr("test", "0xdeadbeef");
        assert!(matches!(r, Err(BoeingBindingsError::BadAddress(_, _))));
    }

    #[test]
    fn decode_addr_rejects_non_hex() {
        let r = decode_addr("test", "0xzzzz");
        assert!(matches!(r, Err(BoeingBindingsError::BadAddress(_, _))));
    }

    #[test]
    fn boeing_bindings_rejects_empty_rpc_url() {
        let r = BoeingBindings::new("");
        assert!(matches!(r, Err(BoeingBindingsError::LiveInit(_, _))));
    }

    #[test]
    fn boeing_bindings_builds_with_dummy_rpc() {
        // No RPC roundtrip yet — Live*::new only validates the URL is
        // non-empty + addresses parse. This guards address parsing
        // for all 20+ contracts.
        let r = BoeingBindings::new("http://127.0.0.1:8545");
        assert!(r.is_ok(), "build err: {:?}", r.err());
    }

    #[test]
    fn boeing_tenant_root_is_stable_keccak() {
        let a = BoeingBindings::boeing_tenant_root();
        let b = BoeingBindings::boeing_tenant_root();
        assert_eq!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
