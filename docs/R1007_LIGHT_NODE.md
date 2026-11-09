---
created: 2026-11-09
branch: feat/r1007-profile
author: Larry Klosowski (MAC lane, 0.5.3 r1007 wiring; Claude Opus 5.5)
status: active (prepared; the r1007 path turns on with the chain pin bump)
---

# The r1007 genesis in the embedded light node

r1007 is the network the 0.5.3 release runs on (chain id 40204, a new block 0). This page says
what the light node has for it today, what stays exactly as it was, and the one change that
turns it on.

## How the embedded node picks its block 0

The embedded node builds block 0 itself when its data dir is empty, and it must build the same
block 0 as the network's nodes or it never syncs. Every build now goes through
`gui/citrate_desktop_app/src/services/genesis_profile.rs`:

| `AppConfig.genesis_profile` | Chain id | Block 0 |
|---|---|---|
| absent (every existing config) | 40204 | `GenesisConfig::testnet_beta()`, as before |
| absent | any other | `GenesisConfig::default()`, as before |
| `"r1007"` | 40204 | the r1007 genesis, in a build with the `chain-r1007` feature; otherwise refused by name |
| `"r1007"` | any other | refused (r1007 runs only on 40204) |
| anything else | any | refused (unknown profile) |

A refusal happens before the node creates or opens its data dir, and it never falls back to
another genesis. Configs without `genesis_profile` load and save unchanged (the field is not
written when it is absent).

## Why the rule is published first

The r1007 block 0 is rooted under the MPT state root rule from block 0. The shared genesis
function in the r1007 chain crates refuses to build the r1007 configuration unless that rule is
already active from block 0 in the process. The network's node publishes it in its own genesis
build; the light node calls the shared function directly, so it has to do the same.

`genesis_profile::build_genesis_block` therefore runs, in this order: the canonical genesis
block; `citrate_execution::mpt_state_root::init_for_genesis(chain id, compiled block 0, None)`;
the shared genesis state; the state root; the block hash. For r1007 it then checks the result
against `R1007_GENESIS`, the block 0 the chain crates compile, and refuses anything else before
it is stored. On start, a data dir whose stored block 0 is not that block is refused too (it
belongs to another network; use a fresh data dir).

The tripwire test `every_genesis_build_publishes_the_rules_first` fails if `node_service.rs`
calls the shared genesis function directly again, or if the r1007 build stops publishing the
rule before it builds the state.

## Pending: the chain pin bump

The workspace pins the citrate-chain crates to `f9f6257b` (`Cargo.toml`), which predates r1007:
it has no r1007 genesis configuration, no `R1007_GENESIS` and no MPT state root rule. The code
that uses them is behind the `chain-r1007` feature of `citrate-desktop-app`, off by default, and
in a default build `"r1007"` is refused with a reason that names this page.

The bump happens when the r1007 genesis commit is merged to public citrate-chain (the same rev
citrate-core's network cut pins as `chainRev`). It is never made to an unpublished rev. In one
change:

1. Move every `citrate-chain` crate rev in the root `Cargo.toml`, including the two
   `[patch."ssh://git@github-citrate-chain/..."]` entries, to that rev, and `cargo update` the
   chain crates. `scripts/ci/check-chain-dep-pins.sh` must stay green (one chain source); the
   `citrate-edu-app` bridge described in `Cargo.toml` moves with it.
2. Turn `chain-r1007` on (make it a default feature, or remove the feature and the
   `cfg(not(feature = "chain-r1007"))` branch of `genesis_state`).
3. API changes at that rev that this crate must follow: `initialize_shared_genesis_state`
   returns a `Result` (already handled in the `chain-r1007` branch), and whatever else changed
   in the chain crates since `f9f6257b` (the rest of the workspace has not been built against
   it).
4. Run the feature's test `r1007_publishes_the_rule_then_builds_the_compiled_block_0`
   single-threaded (the rule is process-wide): it builds the r1007 block 0 through this module
   and checks it equals `R1007_GENESIS`, and checks the shared function refuses without the
   rule.
5. Point the r1007 bootnodes and data dir at the r1007 fleet (`AppConfig.bootnodes`,
   `data_dir`); the bootnode keys are the ones citrate-core's network cut records.

Before the bump, `genesis_profile.rs` was compiled with `chain-r1007` on against the candidate
r1007 chain crates (the r1007 genesis branch before integration), and the feature test passed
there; that rev is not referenced from this repo.

## Not changed here

- The default block 0 for chain 40204 is still `testnet_beta()`, as it was before this change.
  Following a network whose block 0 is another genesis profile needs that profile selected, as
  r1007 now can be once the bump lands.
- No chain crate rev, bootnode or address changes.
