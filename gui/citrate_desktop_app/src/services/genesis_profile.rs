//! Which block 0 the embedded light node builds, and what it publishes first.
//!
//! # Why this module exists
//!
//! The embedded node builds its own block 0 when its data dir is empty, and it must be the
//! same block 0 the network's nodes build (`citrate-chain` `node/src/genesis.rs`), byte for
//! byte, or it never syncs. Until now the choice was inline in `start_node`: chain 40204 built
//! `GenesisConfig::testnet_beta()`, anything else `GenesisConfig::default()`. That is still
//! the behaviour when no genesis profile is configured.
//!
//! The r1007 genesis (0.5.3 network) is selected by name (`AppConfig::genesis_profile =
//! "r1007"`). Its block 0 is rooted under the MPT state root rule from block 0, and the shared
//! genesis function in the r1007 chain crates refuses to build it unless that rule has been
//! published in this process first. So every genesis build goes through
//! [`build_genesis_block`], which publishes the rules keyed to the genesis before it builds.
//!
//! # Pending until the chain pin bump
//!
//! This build's citrate-chain crates (the workspace pin in `Cargo.toml`) predate r1007: they
//! have no r1007 genesis configuration and no MPT state root rule. The r1007 path is therefore
//! compiled only with the `chain-r1007` feature, which is turned on in the same change that
//! bumps the chain crates to the public r1007 genesis commit (`docs/R1007_LIGHT_NODE.md`).
//! Without it, selecting r1007 is refused by name before the node touches its data dir; it
//! never falls back to another genesis.

use crate::error::AppError;

/// The genesis profile name of the r1007 genesis (citrate-chain `genesis::R1007_PROFILE`).
pub const R1007_PROFILE: &str = "r1007";

/// The chain id every Citrate release genesis runs on.
pub const CITRATE_CHAIN_ID: u64 = 40204;

/// Why the r1007 profile is refused by a build without the `chain-r1007` feature.
pub const R1007_PENDING: &str = "the r1007 genesis needs the citrate-chain crates of the r1007 \
     genesis commit, and this build is pinned to an earlier chain rev that cannot build it; it is \
     enabled with the chain pin bump (feature `chain-r1007`, docs/R1007_LIGHT_NODE.md)";

/// The block 0 the embedded node builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenesisChoice {
    /// Chain 40204 with no genesis profile: `GenesisConfig::testnet_beta()` (unchanged).
    TestnetBeta,
    /// Any other chain id with no genesis profile: `GenesisConfig::default()` (unchanged).
    Default,
    /// The r1007 genesis on chain 40204, built under the MPT state root rule from block 0.
    R1007,
}

/// Why no block 0 is chosen. The node does not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenesisRefusal {
    /// A genesis profile this build does not know.
    UnknownProfile(String),
    /// A known profile on a chain id it does not run on.
    WrongChain { profile: String, chain_id: u64 },
    /// A known profile this build cannot build yet.
    Pending { profile: String, why: String },
}

impl std::fmt::Display for GenesisRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProfile(p) => write!(
                f,
                "genesis profile {p:?} is not one this build knows (it knows: {R1007_PROFILE}); \
                 the node is not started"
            ),
            Self::WrongChain { profile, chain_id } => write!(
                f,
                "genesis profile {profile:?} runs only on chain {CITRATE_CHAIN_ID}, not \
                 {chain_id}; the node is not started"
            ),
            Self::Pending { profile, why } => write!(
                f,
                "genesis profile {profile:?} is not available in this build: {why}; the node is \
                 not started"
            ),
        }
    }
}

impl From<GenesisRefusal> for AppError {
    fn from(r: GenesisRefusal) -> Self {
        AppError::Config(r.to_string())
    }
}

/// The block 0 for `chain_id` and the configured genesis profile. No profile keeps the
/// behaviour every existing install has. Pure: decided before the data dir is touched.
pub fn resolve(chain_id: u64, profile: Option<&str>) -> Result<GenesisChoice, GenesisRefusal> {
    match profile.map(str::trim) {
        None | Some("") => Ok(if chain_id == CITRATE_CHAIN_ID {
            GenesisChoice::TestnetBeta
        } else {
            GenesisChoice::Default
        }),
        Some(R1007_PROFILE) => {
            if chain_id != CITRATE_CHAIN_ID {
                return Err(GenesisRefusal::WrongChain {
                    profile: R1007_PROFILE.to_string(),
                    chain_id,
                });
            }
            if cfg!(feature = "chain-r1007") {
                Ok(GenesisChoice::R1007)
            } else {
                Err(GenesisRefusal::Pending {
                    profile: R1007_PROFILE.to_string(),
                    why: R1007_PENDING.to_string(),
                })
            }
        }
        Some(other) => Err(GenesisRefusal::UnknownProfile(other.to_string())),
    }
}

/// Build block 0 for `choice` exactly as the network's node builds it: the canonical genesis
/// block, the shared genesis state (after the genesis-keyed rules are published), its state
/// root, and the block hash over it. The r1007 block is checked against the block 0 the chain
/// crates compile and refused if it differs.
pub fn build_genesis_block(
    choice: GenesisChoice,
    executor: &std::sync::Arc<citrate_execution::executor::Executor>,
) -> Result<citrate_consensus::types::Block, AppError> {
    // Step 1: the canonical genesis block.
    let mut genesis = citrate_economics::genesis::create_canonical_genesis_block(
        citrate_economics::genesis::CANONICAL_GENESIS_TIMESTAMP,
    );
    // Step 2: the genesis-keyed rules, then the shared genesis state (accounts + model).
    let state_root_bytes = genesis_state(choice, executor)?;
    // Step 3: the state root on the genesis block (as the node does).
    genesis.state_root = citrate_consensus::types::Hash::new(state_root_bytes);
    // Step 4: the block hash, with the state root included.
    genesis.header.block_hash =
        citrate_economics::genesis::calculate_canonical_block_hash(&genesis);
    check_block_0(choice, genesis.header.block_hash.as_bytes(), "the built")?;
    Ok(genesis)
}

/// The shared genesis state for `choice` under the chain crates this build is pinned to (before
/// r1007): no genesis-keyed state root rule exists in them, so there is nothing to publish, and
/// r1007 is never chosen ([`resolve`] refuses it).
#[cfg(not(feature = "chain-r1007"))]
fn genesis_state(
    choice: GenesisChoice,
    executor: &std::sync::Arc<citrate_execution::executor::Executor>,
) -> Result<[u8; 32], AppError> {
    let config = match choice {
        GenesisChoice::TestnetBeta => citrate_economics::genesis::GenesisConfig::testnet_beta(),
        GenesisChoice::Default => citrate_economics::genesis::GenesisConfig::default(),
        GenesisChoice::R1007 => {
            return Err(GenesisRefusal::Pending {
                profile: R1007_PROFILE.to_string(),
                why: R1007_PENDING.to_string(),
            }
            .into())
        }
    };
    Ok(citrate_economics::genesis::initialize_shared_genesis_state(
        executor, &config,
    ))
}

/// The shared genesis state for `choice` under the r1007 chain crates. The state root rule is
/// published for the block 0 being built BEFORE the state is built, as the node's genesis build
/// does (`publish_mpt_rule_for_genesis_build`): the r1007 block 0 is rooted under the MPT rule
/// from block 0, and the shared genesis function refuses to build the r1007 configuration while
/// the rule is not active from 0 in this process. Other geneses get their own schedule from the
/// same table (none for testnet_beta), so building them is unchanged.
#[cfg(feature = "chain-r1007")]
fn genesis_state(
    choice: GenesisChoice,
    executor: &std::sync::Arc<citrate_execution::executor::Executor>,
) -> Result<[u8; 32], AppError> {
    use citrate_economics::genesis::GenesisConfig;
    let (config, compiled_block_0, chain_id) = match choice {
        GenesisChoice::TestnetBeta => (GenesisConfig::testnet_beta(), None, CITRATE_CHAIN_ID),
        GenesisChoice::Default => {
            let c = GenesisConfig::default();
            let id = c.chain_id;
            (c, None, id)
        }
        GenesisChoice::R1007 => (
            GenesisConfig::r1007().map_err(|e| AppError::Node(format!("r1007 genesis: {e}")))?,
            Some(citrate_consensus::hardening::R1007_GENESIS),
            CITRATE_CHAIN_ID,
        ),
    };
    let (height, source) = citrate_execution::mpt_state_root::init_for_genesis(
        chain_id,
        compiled_block_0.as_ref(),
        None,
    )
    .map_err(|e| AppError::Node(format!("genesis build: the state root rule: {e}")))?;
    if choice == GenesisChoice::R1007 && height != Some(0) {
        return Err(AppError::Node(format!(
            "genesis build: the r1007 block 0 needs the MPT state root rule from block 0, but it \
             resolved to {height:?} ({source})"
        )));
    }
    tracing::info!("Genesis build: MPT state root from {height:?} (source: {source})");
    citrate_economics::genesis::initialize_shared_genesis_state(executor, &config)
        .map_err(|e| AppError::Node(format!("genesis build: {e}")))
}

/// The block 0 the chain crates compile for `choice`, where they compile one: the r1007
/// genesis (`R1007_GENESIS`) in a `chain-r1007` build. The pinned pre-r1007 crates compile none.
pub fn compiled_block_0(choice: GenesisChoice) -> Option<[u8; 32]> {
    #[cfg(feature = "chain-r1007")]
    if choice == GenesisChoice::R1007 {
        return Some(citrate_consensus::hardening::R1007_GENESIS);
    }
    let _ = choice;
    None
}

/// Block 0 (`got`, built here or found in the data dir) must be the one the chain crates
/// compile for `choice` (when they compile one). Another block 0 is refused: a built one before
/// it is stored, a stored one before the node starts on it (a data dir of another network
/// needs a fresh data dir, as the network's node requires).
pub fn check_block_0(choice: GenesisChoice, got: &[u8; 32], whence: &str) -> Result<(), AppError> {
    check_block_0_against(compiled_block_0(choice), got, whence)
}

fn check_block_0_against(
    want: Option<[u8; 32]>,
    got: &[u8; 32],
    whence: &str,
) -> Result<(), AppError> {
    match want {
        Some(w) if &w != got => Err(AppError::Node(format!(
            "{whence} block 0 {} is not the genesis {} this build compiles for its genesis \
             profile; the node is not started (a data dir of another network needs a fresh \
             data dir)",
            hex::encode(got),
            hex::encode(w)
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No genesis profile keeps today's choice: an existing install builds the block 0 it
    /// always built.
    #[test]
    fn no_profile_keeps_todays_genesis() {
        assert_eq!(resolve(40204, None), Ok(GenesisChoice::TestnetBeta));
        assert_eq!(resolve(40204, Some("")), Ok(GenesisChoice::TestnetBeta));
        assert_eq!(resolve(1337, None), Ok(GenesisChoice::Default));
    }

    /// r1007 is known, runs only on 40204, and (without the r1007 chain crates) is refused by
    /// name, never replaced by another genesis.
    #[test]
    fn r1007_is_known_and_pending_until_the_chain_pin_bump() {
        let e = resolve(40204, Some("r1007"));
        if cfg!(feature = "chain-r1007") {
            assert_eq!(e, Ok(GenesisChoice::R1007));
        } else {
            match &e {
                Err(GenesisRefusal::Pending { profile, why }) => {
                    assert_eq!(profile, "r1007");
                    assert!(
                        why.contains("chain-r1007") && why.contains("R1007_LIGHT_NODE"),
                        "{why}"
                    );
                }
                other => panic!("r1007 must be pending in this build: {other:?}"),
            }
            let msg = AppError::from(e.unwrap_err()).to_string();
            assert!(
                msg.contains("r1007") && msg.contains("not started"),
                "{msg}"
            );
        }
        assert_eq!(
            resolve(1337, Some("r1007")),
            Err(GenesisRefusal::WrongChain {
                profile: "r1007".into(),
                chain_id: 1337
            })
        );
    }

    /// A genesis with a compiled block 0 accepts only that block 0, built or stored; one with
    /// none (every genesis of the pinned pre-r1007 chain crates) accepts what it builds.
    #[test]
    fn block_0_must_be_the_compiled_one_where_there_is_one() {
        let want = [0x4e; 32];
        assert!(check_block_0_against(Some(want), &want, "the stored").is_ok());
        let e = check_block_0_against(Some(want), &[0xa3; 32], "the stored")
            .expect_err("another block 0");
        let msg = e.to_string();
        assert!(
            msg.contains("the stored") && msg.contains(&hex::encode([0xa3u8; 32])),
            "{msg}"
        );
        assert!(msg.contains("fresh data dir"), "{msg}");
        assert!(check_block_0_against(None, &[0xa3; 32], "the stored").is_ok());
        assert_eq!(compiled_block_0(GenesisChoice::TestnetBeta), None);
        assert_eq!(compiled_block_0(GenesisChoice::Default), None);
        assert_eq!(
            compiled_block_0(GenesisChoice::R1007).is_some(),
            cfg!(feature = "chain-r1007")
        );
    }

    #[test]
    fn an_unknown_profile_is_refused() {
        for p in ["r1006", "R1007", "r1007 ", "testnet_beta", "../x"] {
            let r = resolve(40204, Some(p));
            if p.trim() == "r1007" {
                continue;
            }
            assert!(
                matches!(&r, Err(GenesisRefusal::UnknownProfile(_))),
                "{p:?}: {r:?}"
            );
        }
    }

    /// Building the r1007 state without the r1007 chain crates fails closed, even if a caller
    /// skipped `resolve`.
    #[cfg(not(feature = "chain-r1007"))]
    #[test]
    fn building_r1007_without_the_chain_crates_fails_closed() {
        let state_db = std::sync::Arc::new(citrate_execution::StateDB::new());
        let executor = std::sync::Arc::new(citrate_execution::executor::Executor::new(state_db));
        let e = build_genesis_block(GenesisChoice::R1007, &executor)
            .err()
            .expect("refused");
        assert!(e.to_string().contains("r1007"), "{e}");
    }

    /// With the r1007 chain crates: the light node publishes the MPT state root rule from
    /// block 0 and then builds exactly the block 0 the chain compiles (`R1007_GENESIS`), and the
    /// shared genesis function refuses the r1007 configuration while the rule is not published
    /// (the failure this module exists to prevent). Run single-threaded: the rule is
    /// process-wide.
    #[cfg(feature = "chain-r1007")]
    #[test]
    fn r1007_publishes_the_rule_then_builds_the_compiled_block_0() {
        let executor = || {
            let db = std::sync::Arc::new(citrate_execution::StateDB::new());
            std::sync::Arc::new(citrate_execution::executor::Executor::new(db))
        };
        citrate_execution::mpt_state_root::set_mpt_state_root_height(None);
        let cfg = citrate_economics::genesis::GenesisConfig::r1007().expect("r1007 config");
        let refused =
            citrate_economics::genesis::initialize_shared_genesis_state(&executor(), &cfg);
        assert!(
            refused.is_err(),
            "the shared build must refuse r1007 without the rule"
        );

        let block = build_genesis_block(GenesisChoice::R1007, &executor()).expect("r1007 block 0");
        assert_eq!(
            block.header.block_hash.as_bytes(),
            &citrate_consensus::hardening::R1007_GENESIS
        );
        assert_eq!(
            citrate_execution::mpt_state_root::mpt_state_root_height(),
            Some(0)
        );
    }

    /// The block 0 this module builds for 40204 with no profile is the one `start_node` built
    /// inline before (same steps, same order): building it twice gives the same hash, and the
    /// state root is set on it.
    #[cfg(not(feature = "chain-r1007"))]
    #[test]
    fn the_default_40204_block_0_is_unchanged_and_deterministic() {
        let build = || {
            let state_db = std::sync::Arc::new(citrate_execution::StateDB::new());
            let executor =
                std::sync::Arc::new(citrate_execution::executor::Executor::new(state_db.clone()));
            let mut inline = citrate_economics::genesis::create_canonical_genesis_block(
                citrate_economics::genesis::CANONICAL_GENESIS_TIMESTAMP,
            );
            let other_db = std::sync::Arc::new(citrate_execution::StateDB::new());
            let other = std::sync::Arc::new(citrate_execution::executor::Executor::new(other_db));
            let root = citrate_economics::genesis::initialize_shared_genesis_state(
                &other,
                &citrate_economics::genesis::GenesisConfig::testnet_beta(),
            );
            inline.state_root = citrate_consensus::types::Hash::new(root);
            inline.header.block_hash =
                citrate_economics::genesis::calculate_canonical_block_hash(&inline);
            let built = build_genesis_block(GenesisChoice::TestnetBeta, &executor).expect("built");
            (
                inline.header.block_hash,
                built.header.block_hash,
                built.state_root,
            )
        };
        let (inline, built, root) = build();
        assert_eq!(
            built, inline,
            "the module builds the block start_node built inline"
        );
        assert_ne!(root, citrate_consensus::types::Hash::default());
        assert_eq!(build().1, built, "deterministic");
    }

    /// Tripwire: every genesis build of the embedded node goes through this module, and the
    /// r1007-capable build publishes the state root rule before it builds the shared genesis
    /// state (the shared function refuses r1007 otherwise; a GUI or light node that forgets
    /// fails loudly there, this keeps it from being forgotten).
    #[test]
    fn every_genesis_build_publishes_the_rules_first() {
        let here = include_str!("genesis_profile.rs");
        let node = include_str!("node_service.rs");
        let call = concat!("initialize_shared_genesis_state", "(");
        assert!(
            !node.contains(call),
            "node_service.rs builds a genesis state directly; use genesis_profile::build_genesis_block"
        );
        assert!(node.contains("genesis_profile::build_genesis_block("));
        let gated = here
            .split("#[cfg(feature = \"chain-r1007\")]\nfn genesis_state(")
            .nth(1)
            .expect("the chain-r1007 genesis_state");
        let body = &gated[..gated.find("\n}\n").expect("end of fn")];
        let publish = body
            .find(concat!("mpt_state_root::init_for_genesis", "("))
            .expect("the r1007 build publishes the MPT state root rule");
        let build = body
            .find(call)
            .expect("then builds the shared genesis state");
        assert!(
            publish < build,
            "the rule is published before the state is built"
        );
    }
}
