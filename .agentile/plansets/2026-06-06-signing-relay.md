---
created: 2026-06-06T00:00:00Z
branch: main
author: Saul Loveman + Claude Opus 4.8
status: planset (Stage-2, red-teamed)
planset: gtm-spine
code: GUI-RELAY (SELL-S2 signer half / closes TD-17 + TD-27)
repo: citrate-native
red_teamed: 2026-06-06
---

# Planset — gui-native Signing Relay (the SELL-S2 keystore signer)

> **One-line goal:** make won marketplace jobs actually advance **on-chain** by
> giving the desktop wallet a background loop that drains the node-agent's
> `GET /signature-requests` queue, **signs each write with the existing keystore**,
> broadcasts it, and reports it observed. This is the *last* piece between
> "node-agent executes in tests" and "the seller earns on testnet."

## 0. Why this is small (the reuse map)

gui-native already has the entire hard part — signing + broadcast. This feature
is **mostly glue**. Verified surfaces:

| Need | Already exists in gui-native | Location |
|---|---|---|
| Get a signing key (unlocked) | `KeyManager::get_signing_key(addr) → UnifiedKey` (Ed25519/secp256k1) | `desktop_app/src/services/wallet_service.rs:177` |
| Build + sign + broadcast a **contract call** | `WalletService::send_transaction_with_data(from, to, value_wei, data: Vec<u8>, _pw)` — nonce-fetch → `TransactionBuilder.sign[_secp256k1]` → `rpc.send_raw_transaction` | `wallet_service.rs:219–255` |
| Account nonce | `rpc.get_nonce(from)` (per send) | `wallet_service.rs:184` |
| RPC client (chain 40204) | `citrate_wallet_core::RpcClient` (`send_raw_transaction`, `get_nonce`) | `wallet_service.rs:70–96` |
| Canonical address book | `compute_marketplace_address(40204)`, `contribution_accounting_address(40204)`, … | `gui_native/src/marketplace_client.rs:24–134` |
| Background loop pattern | balance-refresh thread (`std::thread::spawn` loop + `slint::invoke_from_event_loop`) | `gui_native/src/main.rs:2672–2877` |
| Service + injectable backend + tests | `WalletService` / `WalletBackend` trait + mock backend | `desktop_app/src/services/`, `tests/wallet_integration.rs` |
| HTTP client | `reqwest` already a dep | `Cargo.toml` |

**The relay write IS a contract call with calldata** — so `send_transaction_with_data`
is a near-perfect fit: `to = req.to`, `value_wei = req.value_wei`, `data = hex_decode(req.calldata)`.

## 1. The contract we consume (node-agent, already shipped on main)

The node-agent side is built (`citrate-node-agent` `relay.rs` + `supervision`), default
loopback `http://127.0.0.1:19600` (`CITRATE_NODE_AGENT_ADDR`):

- `GET /signature-requests` → JSON array of
  `{ id:u64, intent, to(0x), calldata(0x), value_wei(decimal string), chain_id:u64,
     context, expires_block(string), status:"pending"|"submitted", tx_hash? }`.
- `POST /signature-requests/{id}/observed` body `{ "tx_hash": "0x…" }` → marks it submitted.

The node-agent **dedups by calldata** and advances on **chain truth**, so the relay is
free to be conservative (re-polling / one-at-a-time) without causing double-broadcast.

## 2. Red-team — corrected assumptions (supersede the naive plan)

1. **No node-agent supervision exists in gui-native today.** The GUI embeds a chain
   node but does **not** spawn/talk to the node-agent. → This planset builds the
   **signer (HTTP client to an already-running node-agent)**; *spawning/supervising the
   node-agent process* is a **separate follow-on** (GUI-RELAY-S2 / TD-11), out of scope here.
2. **Signing requires an unlocked wallet.** `get_signing_key` fails when locked. → The
   relay can only sign while the wallet session is unlocked. Design: the relay is
   **opt-in** (a toggle) **and** only acts while unlocked; when locked it idles (it does
   *not* prompt for the password — auto-unlock for a background signer would be a custody
   footgun). Honest UX: surface "relay active / paused (locked)".
3. **Nonce races.** `send_transaction_with_data` fetches the nonce per call; firing two
   writes in one tick can collide. → Process **at most one pending request per poll
   cycle** (the node-agent emits writes sequentially per job + dedups, so one-per-tick
   still converges). Revisit a local nonce manager only if throughput demands it.
4. **At-most-once.** Only act on `status == "pending"`; POST `observed` **only after** a
   successful broadcast (tx_hash in hand). On broadcast failure, leave it pending → retry
   next cycle. Never POST observed without a real hash.
5. **These writes move money/escrow.** A compromised or buggy node-agent could enqueue a
   malicious `to`/calldata. → **Validate every request before signing** (see §4). The
   relay signs *only* the five known SELL-S2 writes to the two known contracts, value 0.
6. **Auto-signing posture (ADR-agent-signing).** Auto-signing the node's *own*
   job-execution writes is the sanctioned operator path — but it must be **explicit,
   bounded, and visible**: opt-in toggle, validated allow-list, value-0 only, a visible
   log/toast of what was signed. No silent blanket signing.

## 3. Architecture

```
desktop_app/src/services/relay_service.rs   (NEW — headless, testable)
  ├─ NodeAgentClient (reqwest): list_pending() , mark_observed(id, tx_hash)
  ├─ RelayValidator: chain_id==40204 && to∈allow-list(intent) && value_wei=="0"
  │                   && calldata[0..4]==expected_selector(intent)
  ├─ RelayConfig: { agent_url, poll_interval, enabled }
  └─ run_once(&wallet, &client, &validator) -> RelayTickReport
       for the first `pending` req that validates:
          data = hex_decode(req.calldata)
          tx_hash = wallet.send_transaction_with_data(active_from, req.to, req.value_wei, data, "")
          client.mark_observed(req.id, tx_hash)
gui_native/src/main.rs
  └─ background thread (mirror balance-refresh loop): every N s, if relay.enabled
     && wallet.is_unlocked() → block_on(relay.run_once(...)); push a toast/log line.
  └─ Settings toggle "Auto-sign node-agent jobs" + status indicator.
```

- **Layering:** all logic in `desktop_app` (headless, unit-tested with a mock node-agent
  + the existing mock `WalletBackend`); the Slint app only owns the toggle + the loop
  spawn + status surfacing. Mirrors the `WalletService` pattern exactly.
- **No new keystore code.** Reuse `send_transaction_with_data` verbatim.

## 4. Safety validation (refuse to sign unless ALL hold)

Per request, before signing:
- `chain_id == active chain_id` (40204).
- `value_wei == "0"` (all five writes are non-payable).
- `to` matches the intent's contract: `startExecution|submitCommitment|submitResult|completeJob`
  → `compute_marketplace_address(40204)`; `claimRewards` → `contribution_accounting_address(40204)`.
- `calldata` 4-byte selector matches the intent
  (`startExecution=0xc78ec18e`, `submitCommitment=0xe6a3d9dc`, `submitResult=0xbaa2c078`,
  `completeJob=0xa1c0d32f`, `claimRewards=0x372500ab`).
- (advisory) skip if `expires_block` is already past the chain head.
A request failing any check is **skipped + logged** (never signed); it stays pending so a
human can inspect. This is the trust boundary against a compromised node-agent.

## 5. Scope

**In (GUI-RELAY-S1):** `relay_service` (NodeAgentClient + validator + run_once); the
opt-in background loop + Settings toggle + status/log surfacing; unit tests (mock
node-agent + mock wallet) for poll→validate→sign→observe, the validation refusals, the
locked/disabled idle paths, and at-most-once; an anvil/devnet integration test (gated on
env) driving one real write end-to-end.

**Out (follow-ons):** node-agent **process spawning/supervision** (GUI-RELAY-S2 / TD-11);
multi-account / hardware-wallet signing; a local nonce manager (only if throughput needs
it); gas-price strategy beyond the existing builder default; reserved-capacity / batch.

## 6. Sprint breakdown (riskiest first)

| Sprint | Focus | Gate |
|---|---|---|
| **S1.1** | `relay_service` types + `NodeAgentClient` (reqwest) + `RelayValidator` — pure/HTTP, no UI | Unit: parse the queue JSON; validator accepts the 5 valid writes, refuses bad `to`/value/selector/chain. |
| **S1.2** | `run_once` wiring to `WalletService::send_transaction_with_data` + `mark_observed` | Mock node-agent + mock wallet: a valid pending → signed → broadcast → observed POST with the hash; broadcast failure → stays pending, no observe. |
| **S1.3** | Background loop + Settings toggle + status/log; unlock-gated + opt-in | Loop idles when disabled or locked; activates when enabled + unlocked; a toast names each signed intent. |
| **S1.4** | Devnet/anvil e2e (env-gated) — one real `startExecution`/`claimRewards` advances on-chain | `CITRATE_RELAY_E2E` run: a pending write lands a real tx; node-agent sees the state advance next tick. |

## 7. BDD (author before each sprint)

- `Given the wallet is unlocked and the relay is enabled, And node-agent has a pending submitResult for ComputeMarketplace with value 0, When the relay ticks, Then it signs + broadcasts it and POSTs observed with the tx hash.`
- `Given a pending request whose 'to' is not a known SELL-S2 contract, When the relay ticks, Then it refuses to sign, logs the refusal, and leaves it pending.`
- `Given the wallet is locked, When the relay ticks, Then it signs nothing and reports 'paused (locked)'.`
- `Given a request already 'submitted', When the relay ticks, Then it is not re-broadcast.`
- `Given broadcast fails, When the relay ticks, Then no observed POST is sent and the request remains pending for retry.`

## 8. Residual risks

- **R1 — custody/auto-sign blast radius.** Mitigated by opt-in + unlock-gating + the §4
  allow-list (value-0, known contracts/selectors only) + visible per-sign log. Flag for
  citrate-security review before a funded testnet run.
- **R2 — node-agent availability.** If 19600 is down, the relay logs + backs off; it never
  blocks the UI (runs on the background thread, like balance refresh).
- **R3 — nonce under bursts.** One-write-per-tick avoids it for MVP; local nonce manager is
  the noted upgrade (R3 in the node-agent WP-C handoff mirrors this).
- **R4 — node-agent not running / not GUI-managed.** S1 assumes an already-running
  node-agent (operator launches it / future GUI-RELAY-S2 spawns it). Documented, not hidden.

## 9. Definition of done (S1)
A user who unlocks their wallet and flips "Auto-sign node-agent jobs" sees their node's
won jobs walk `startExecution → submitCommitment → submitResult → completeJob` and
`claimRewards` to payout on devnet — each tx visible in the activity log — with the relay
refusing anything outside the validated allow-list. Closes the gui-native half of TD-17 /
TD-27; pairs with `citrate-labs handoffs/GUI_NATIVE_SIGNING_RELAY_HANDOFF.md`.

## Execution status (2026-06-06)
- **S1.1 ✅** `PendingRequest` + `RelayValidator` (allow-list) + `NodeAgentClient` — 16 tests.
- **S1.2 ✅** `run_once` (poll→validate→sign-one/tick→observe-after-hash) over `TxSigner`/`RequestQueue` mocks.
- **S1.3 ✅** `RelayService` opt-in/unlock gate (`tick`) + `WalletTxSigner` over `WalletService` (no new keystore code) + the gui_native background loop + toast (19 relay tests total).
- **S1.3b ✅** "Auto-sign won jobs" toggle in the Compute panel (compute.slint + app.slint + main.rs), bound to the shared `RelayService` flag.
- **S1.4 ✅** env-gated (`CITRATE_RELAY_E2E`) read-only e2e against a live node-agent.
- **Remaining = operator DoD (not code):** the full sign→broadcast→observe→payout on a devnet with a won job + funded unlocked wallet (planset §9). Also a future hardening item for citrate-security review: the relay's `send` touches the backend session (auto keep-alive) — bounded by the GUI session clock + opt-in + value-0 allow-list (R1).

## 10. Cross-refs
node-agent: `relay.rs` (RelaySigner), `supervision/src/{state,server}.rs` (queue + endpoints).
Handoff: `citrate-labs/handoffs/GUI_NATIVE_SIGNING_RELAY_HANDOFF.md`.
Tech-debt: TD-17, TD-27 (federation register).
