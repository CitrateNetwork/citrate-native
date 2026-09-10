---
created: 2026-07-04T00:00:00Z
branch: main
author: Larry Klosowski (@SaulBuilds) + Claude Fable 5
status: draft
program: NATIVE-R1 (native desktop revamp)
code: NATIVE-R1-S3 (PIN storage rebuild)
repos: citrate-gui-native (Storage tab, PinSigner, runloop embed) + citrate-node-agent (chainio encoders, pinning crates)
companion: ../../../handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md (§6 IPFS→PIN rebuild)
depends-on: 2026-07-04-native-r1-s2-qa-harness.md (WP-5 honest deploy-model stub — this planset replaces that flow); PIN program plansets in citrate-federation (.agentile/planset/2026-06-03-pin-provable-storage.md and successors)
supersedes: the Storage tab "local file manager" design; the unwired models-deploy-model flow
contract: IPFSIncentivesV3 @ 0x629f7cd4aeade49e4b27c9a39237d132f9ff39f4 (chain 40204)
---

> **Status (2026-07-04): DRAFT.** Source of truth:
> `handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md` §6. The PIN system is ~90% done —
> V3 deployed, sealed PoRep + rolling PoSt via precompile 0x0108, KYC-gated pinners,
> commit-reveal challenges. **This planset is the remaining 10%: the client side**,
> and the desktop app is the intended client — the daemon is **keyless by design**
> and the native wallet is its signer. This is the 8-item delta, no more.

# Planset — NATIVE-R1-S3: Storage tab rebuilt against IPFSIncentivesV3

> **Goal.** The Storage tab stops being a chain-blind local file manager and becomes
> the PIN client: a KYC'd user pins registry CIDs for reward and watches vesting /
> challenge deadlines live; a publisher registers a model with its CommD bond — all
> signed by the app wallet through the daemon's keyless signature seam.

## Scope (in) — the 8-item delta
V3 chainio encoders (node-agent) · PinSigner via the app wallet/AA stack · runloop
embed + live PinChainView · KYC gate UI · two-view Storage redesign (pin-for-reward
+ publish-a-model) · kubo kept as byte layer · sealer binary packaging ·
verified-weights fetch path.

## Scope (out) — honest boundaries
- **Real-size VK regeneration (TD-19)** — DGX-gated, explicitly OUT; S3 ships
  functional on the reduced VK and says so in the UI/docs where proof strength matters.
- **Sybil binding** — `setSybilBinding(true)` stays OFF pending IDP-S3 real `sub`
  claims (PIN-S4 planset); S3 builds nothing that assumes it on.
- **V3 contract changes** — the contract is deployed; Rule-8/Slither hardening is
  staged in citrate-security #7, not here.
- **Replacing kubo** — kubo (127.0.0.1:5001) remains the byte layer; no custom IPFS.
- **Sealing-as-a-service operations** — SaaS pool economics live in the PIN-SAAS
  planset; S3 only packages/locates the sealer binary the client needs.

## Work packages (riskiest first)

Acceptance criteria name their data source per Rule 11.

| WP | Title | Repo | Acceptance (with data source) |
|---|---|---|---|
| **WP-1** | **V3 calldata encoders** — `registerModel`, `commitChallenge`, `challengePin`, `recordSealerProof`, V3 `submitPoSt` added to `crates/chainio` | citrate-node-agent | Each encoder's output byte-matches an ABI fixture generated from the V3 artifact (selector + args), and a testnet `eth_call`/dry-run against `0x629f…39f4` decodes cleanly. **Source:** the deployed V3 ABI (chain 40204) + fixture vectors checked into chainio tests. |
| **WP-2** | **PinSigner over the keyless seam** — the daemon emits unsigned `PinSignatureRequest`s; implement the signer against the app wallet/AA stack (pattern: `.agentile/plansets/2026-06-06-signing-relay.md` — drain queue → sign with keystore → broadcast → report observed) | gui-native | An emitted `PinSignatureRequest` is signed, broadcast, and confirmed; the daemon never touches key material (code-review assertion + no-key-in-daemon test). Locked wallet ⇒ requests queue, never auto-sign. **Source:** the daemon's signature-request queue + tx receipts from the embedded node RPC. |
| **WP-3** | **Pinning runloop embedded** — wire `crates/pinning/{lib,runloop,sidecar}.rs` (currently in no binary) into the desktop app service layer, fed by a **live PinChainView** (V3 events/state, not polling stubs) | gui-native + node-agent | With kubo up and a registered pinner: runloop pins a registry CID, answers a rolling challenge, and the claimable balance moves. **Source:** V3 contract state (`getPin`, `owedOf`, challenge windows) read over the embedded node — never a local cache presented as chain truth. |
| **WP-4** | **KYC gate UI** — V3 pinners are KYC-gated; surface the gate before `registerPinner` | gui-native | Un-KYC'd user sees the gate + a working handoff to auth.citrate.ai (reuse the CITRATE IDENTITY card wiring, AUTHSPINE S3-WP3); KYC'd user proceeds; state never inferred client-side. **Source:** the AUTHSPINE entitlement claim via the identity service + the V3 contract's own pinner-eligibility check (both must agree; contract wins). |
| **WP-5** | **Two-view redesign, view A: Pin-for-reward** — my pins, slot status, vested/claimable, challenge deadlines, slash risk; pin/unpin actions route through the runloop | gui-native | Every displayed field names its getter (`getPin`, `owedOf`, challenge schedule) in code; snapshot tests per state (empty/active/challenged/slashed) on the S1 theme. **Source:** V3 contract reads via PinChainView. |
| **WP-6** | **Two-view redesign, view B: Publish-a-model** — `registerModel` + CommD bond status; **this replaces the unwired `models-deploy-model`** (S2 WP-5's honest stub is deleted here) | gui-native | A model registration lands on V3 (WP-1 encoder + WP-2 signer), bond status displays from chain; the dead `models-deploy-model` callback is removed from `app.slint` and the Models screen links here. **Source:** V3 `registerModel` receipt + registry/bond state. |
| **WP-7** | **Sealer binary packaging** — ship/locate `citrate-sealer` via `CITRATE_SEALER_BIN` (env-with-default), per-platform packaging note (macOS + Linux CI targets) | gui-native | Fresh install resolves the sealer (bundled or discovered), and a seal round-trips through the sidecar in the integration test; missing binary ⇒ actionable error, not a hang. **Source:** the sidecar's seal-proof output verified by the 0x0108 precompile path in a testnet dry-run (reduced VK, per scope-out). |
| **WP-8** | **Verified-weights fetch path** — jobs/models fetch weights by CID with V3-backed verification (the PIN promise: a job can always fetch verified weights) | gui-native | Fetching a registered model verifies content against its registered CommD/CID before use; tamper fixture fails closed. **Source:** V3 model registry entry + kubo block fetch, verified locally. |
| **WP-9** | **PIN integration test lane** (§8 item 5) — mock chain view + fake sidecar sealer for CI; the live path stays behind the S2 smoke lane | gui-native | CI runs the runloop against the mock PinChainView + sealer fake deterministically; the same scenario script runs (gated) against testnet. **Source:** mock fixtures derived from recorded V3 testnet responses (recorded, not invented). |

## Dependency table (cross-planset / cross-repo)

| Edge | Direction | Why |
|---|---|---|
| **WP-1 (chainio) is in citrate-node-agent** | S3 → node-agent | the encoders land there and flow back via the node-agent dependency — `[[drift]]` gui-native ← node-agent, node-agent ← citrate-chain (V3 ABI); manifest entries first (Rule 12) |
| NATIVE-R1-S2 WP-5 → this WP-6 | supersession | S2 stubs the dead deploy-model button; S3 deletes the stub and ships the real flow |
| PIN program (federation plansets) → S3 | upstream | contract/daemon/seam are PIN-S1/S2/S6 deliverables; S3 consumes, does not modify |
| IDP-S3 → Sybil binding | explicitly NOT a dependency | binding stays off; S3 must work either way |
| NATIVE-R1-S1 theme → WP-5/6 snapshots | prerequisite | snapshot acceptance is against the evergreen theme |
| VALIDATOR-S1 Phase 4 ← S3 | downstream | pinning earnings become a named contribution stream in the honest-earnings display |

## Sequencing
WP-1 first (everything encodes through it), WP-2 next (everything signs through
it); WP-3 then unlocks WP-5; WP-6 after WP-1+2; WP-4 parallel-safe; WP-7/8 late;
WP-9 grows alongside from WP-3 on.

## Honest sizing
~2–3 weeks. Risk concentrates in WP-3 (the runloop has never run inside a binary)
and WP-7 (sealer packaging across platforms). The reduced-VK caveat (TD-19) is a
**known, declared** proof-strength limitation, not a hidden one.

## References / rules
- Master brief: `/home/saul/Projects/Citrate-Labs/handoffs/CITRATE_NATIVE_REVAMP_MASTER_BRIEF.md` (§6, §8 item 5, §9)
- PIN lineage: `citrate-federation/.agentile/planset/2026-06-03-pin-provable-storage.md`, `2026-06-07-pin-cr-s1-v3-contract.md`, `2026-06-07-pin-saas-s1-sealing-pool.md`
- Signer pattern: `.agentile/plansets/2026-06-06-signing-relay.md` (this repo)
- Rules: `citrate-federation/.agentile/rules/CORE_RULES.md` — Rule 1 (no mock chain
  views in prod paths), Rule 10 (wallet approval before signing), Rule 11 (data
  sources above), Rule 12 (drift entries before the cross-repo dep lands).
