---
created: 2026-06-10T00:00:00Z
branch: audit/secrem02-signing-seam
author: Fable 5 (Claude Code)
sprint: SECREM-02-followup-remediation
status: active
repo: citrate-native
baseline_test_count: 520
---

# citrate-native — SECREM-02 Remediation Log

> Coverage matrix: `citrate-security/planset/2026-06-10-followup-remediation.md`.
> The other half of the **Phase-2 signing seam** (with citrate-node-agent).
> Protocol: re-verify → red test → fail-closed fix → suite green → mutation pass.

## Phase 2 — signing-seam (gui-native side)

| Finding | Sev | WP | Red test(s) | Fix | Suite | Disposition |
|---|---|---|---|---|---|---|
| FUA-GUI-02 | Med | 2.1 | `relay_service.rs::validate_node_agent_url_accepts_loopback` / `…_rejects_remote_and_non_http` | `validate_node_agent_url` rejects non-loopback host + non-`http` scheme; `NodeAgentClient::try_new` enforces it; `main.rs` uses `try_new` and **does not start the relay** against a bad node-agent address — `gui/citrate_desktop_app/src/services/relay_service.rs`, `gui/citrate_native/src/main.rs` | relay tests 21 ✓ | **FIXED** |
| FUA-GUI-01 (auth half) | High | 2.1 | covered by node-agent gate tests + the relay continuing to function with the token | `NodeAgentClient` now loads the shared supervision token and presents `Authorization: Bearer` on every request (`authed()`), so the seam keeps working now that the node-agent surface is gated. The queue is no longer reachable by an unauthenticated process — the precondition that made FUA-GUI-01 reachable is closed. | 21 ✓ | **PARTIAL — auth closed; calldata-arg + per-write confirm = follow-up** |

## FUA-GUI-01 residual (tracked, not yet done)

The relay's `validate()` already refuses any **non-zero-value** write and checks
the **contract + 4-byte selector**, so the auto-sign surface is narrow; and the
node-agent queue is now **authenticated** (a malicious local process can't inject
into it). The remaining defense-in-depth — **ABI-decoding the calldata arguments**
and cross-checking them against the job context, plus a **per-write human
confirmation** for value-bearing writes in the Slint UI — is a larger change and
is carried forward as a focused follow-up under WP 2.1. With the queue
authenticated, its residual risk (a compromised/buggy node-agent feeding bad args)
is materially lower than the original "any local process" exposure.

## Notes
- Baseline (Phase 0): gui-native workspace **520**; relay_service tests **19 → 21**
  (+2 loopback-enforcement tests). Rule 2 satisfied.
- Shared contract: token file at `CITRATE_NODE_AGENT_TOKEN_FILE` /
  `$HOME/.citrate/node-agent/supervision.token` — same as node-agent.
- `NodeAgentClient::new` stays infallible (tests/e2e use a loopback mock);
  `try_new` is the production constructor that enforces loopback.
- Branch: `audit/secrem02-signing-seam`.

## Phase 6 — WP 6.4b (FUA-GUI-01 residual + still-open prior findings)

> Branch: `audit/secrem02-residuals-6-4b`, 2026-06-11.
> Baseline at WP start (`cargo test --workspace --no-fail-fast` @ main `2db8bd5`):
> **522 total = 519 passed + 3 PRE-EXISTING failures** (marketplace_client
> canonical-address tripwires — see "Pre-existing failures" below).
> End state: **546 passed, 0 failed** (+24 tests, 11 RED-first).

| Finding | Sev | Red test(s) (confirmed failing first) | Fix | Mutation | Disposition |
|---|---|---|---|---|---|
| FUA-GUI-01 residual (a): calldata args unvalidated | High | `relay_service` → `validator_refuses_claim_rewards_with_unexpected_args`, `…_start_execution_with_trailing_bytes`, `…_truncated_submit_commitment`, `…_submit_result_with_bogus_offsets`, `…_implausible_job_id` (5 RED) | `decode_args()` strict per-selector ABI shape validation (exact static lengths; canonical in-bounds `submitResult` dynamic offsets + exact total; u128-bounded job ids; 128 KiB calldata cap); wired into `RelayValidator::validate`; `ValidatedWrite.args: DecodedArgs` + `describe()` | killed: claimRewards-accepts-args mutant → 1 FAIL | **FIXED** |
| FUA-GUI-01 residual (b): no per-write confirmation | High | `requires_confirmation_policy`, `run_once_refuses_unconfirmed_claim_rewards`, `run_once_confirmed_claim_rewards_signs_and_lifecycle_skips_gate`, `deny_all_gate_refuses` | Pure `requires_confirmation(intent, value_wei)` (claimRewards + any value-bearing write); `ConfirmationGate` seam on `run_once`/`tick` with fail-closed `DenyAllConfirmations`; `main.rs` `RelayApprovalGate` → existing `PendingApprovalStore` (Operations panel Approve/Deny, 60 s auto-deny, declined-id muting so a stuck queue entry can't prompt-storm) | killed: `requires_confirmation → false` mutant → 4 FAIL | **FIXED** (value-0 job-lifecycle writes remain auto-signed under the explicit relay opt-in — by ADR-agent-signing design) |
| 002 (MED) malformed `execute(ForwardRequest,bytes)` encoder | Med | `execute_encoding_is_canonical_abi`, `execute_encoding_rejects_malformed_fields` (2 RED) | Canonical encoding (exact `sig_offset = 0x40 + tuple_size`, no "approximate"); `abi::require_hex` validates bytes32/address fields; `get_nonce` validates its bytes32 too | killed: +32 sig_offset mutant → 1 FAIL | **FIXED** |
| 003 (MED) edu `abi.rs` string-pad + u64 truncation | Med | `test_decode_uint8_rejects_oversized` (RED), `test_decode_uint256_rejects_garbage`, `test_decode_uint256_wide_values`, `test_encode_call_address_validates_input` | `decode_uint256 → Option<u128>` (strict hex, ≤ 1 word, > u128 refused); `decode_uint64`/`decode_uint8` checked; `encode_call_address`/`encode_call_uint256_address → Result` with validated 20-byte addresses; all edu-service callers updated fail-closed | killed: `as u8` truncation mutant → 1 FAIL | **FIXED** |
| 004 (LOW) `registerModel` len-as-u8 + encoder/decoder sig drift | Low | `app_binder::register_model_length_word_is_not_truncated` (RED), `register_model_layout_is_canonical`, `calldata_decoder::model_registry_register_model_resolves` (RED) | Encoder extracted to `encode_register_model_calldata` with full u64 BE length word; `registerModel(bytes32,string)` added to the decoder registry (BFR `bytes32,bytes32` kept — distinct selector) | killed: `len() as u8` mutant → 1 FAIL | **FIXED** |
| 005 (LOW) MCP `session/end` no caller-ownership binding | Low | `test_005_session_end_without_owner_token_is_refused` (RED), `…_with_owner_token_succeeds`, `…_tokenless_grant_can_end_with_grant_id_alone` | `grant_owners` map records the minting auth_token at `initialize`; `session/end` for a token-minted grant requires the same token (params or transport bearer); tokenless ReadOnly grants keep grant-id-as-credential | killed: drop-ownership-check mutant → 1 FAIL | **FIXED** |
| 006 (INFO) `chain_id_for_network("mainnet") == 1` | Info | `test_chain_id_mainnet_alias_is_not_ethereum` (RED) | Every alias resolves to the permanent 40204; the Ethereum-replayable "reserved" 1 is gone | killed: `mainnet => 1` mutant → 1 FAIL | **FIXED** |
| FUA-GUI-03 (LOW) non-numeric `value_wei` fails open | Low | `test_fua_gui_03_non_numeric_value_fails_closed` (RED); `security_edge_cases` re-pinned fail-closed (negative/non-numeric/empty/>u128 now `Err`) | `enforce_value_reauth` parses fail-closed per the WP-001 DoD (scope addition — small, on the signing chokepoint) | killed: `unwrap_or(0)` mutant → 1 FAIL | **FIXED** (scope addition) |
| FUA-GUI-04 (LOW) relay ignores `expires_block` | Low | — | — | — | **DEFERRED-WITH-OWNER** — owner: citrate-native maintainer (relay WP backlog). Reason: needs a current-block input plumbed into the relay tick (RPC dependency); belt-and-braces only (stale writes waste gas / late-submit risk), now further mitigated by arg validation + the authenticated queue. Not in the WP 6.4b mandate (new-finding, not prior). |

### Pre-existing failures resolved (not an audit finding)
`marketplace_client` canonical-address tripwires (3 tests) were already failing
on main `2db8bd5`: the vendored `generated/addresses.json` (post-reroll) and the
test pins/`citrate-chain` `DEPLOYED_ADDRESSES.md` (pre-reroll) disagreed.
Resolved against the LIVE chain on 2026-06-11: `eth_getCode` via
`https://rpc.citrate.ai` returns bytecode at `0xf62ab4…5283`
(ComputeMarketplace per addresses.json) and `0x` (no contract) at the old
`0xc12dbc…373c` pin. Test pins re-pinned to the live set.
**Flag to citrate-chain:** `contracts/DEPLOYED_ADDRESSES.md` still carries the
stale pre-reroll table (incl. the ComputeMarketplace/ContributionAccounting
addresses the 2026-06-09 audit reports quote) and needs its own reconcile.

### Notes (WP 6.4b)
- Suite: 519 passed/3 failed → **546 passed/0 failed** (14 test binaries).
- relay_service tests 21 → 31.
- `async-trait` promoted from dev-deps to deps in `citrate-native` (gate impl
  in `main.rs`).
- All 8 mutants killed; tree restored from the fix commit after each.
