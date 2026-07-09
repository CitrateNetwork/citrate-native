# Known issues — citrate-native beta

Honest list, per the NATIVE-R1-S2 release method: nothing in the demo path
lies, and anything that doesn't work yet is named here instead of hidden.
Every line traces to the revamp master brief §2 or the 2026-07-04 owner QA
survey. Last updated 2026-07-09.

## What this beta guarantees (verified, not aspirational)

- **Encryption at rest is real and on by default.** The embedded node's
  RocksDB is encrypted under an OS-keyring master key; tests prove raw
  on-disk bytes are sealed ciphertext with no plaintext findable. An
  existing unencrypted store from an older build is wiped and re-synced
  automatically on first start (public chain data — nothing of yours is
  lost; your wallet keystore is a separate, untouched store).
- **The node connects and stays connected.** All four testnet-beta
  bootnodes handshake (Noise-encrypted) and hold; the earlier
  drop-after-handshake bug was a stale-genesis pin, fixed and covered by a
  live smoke test.
- **If the node ever crashes, it says why.** Any node-thread death writes
  a crash record (backtrace, last log lines, build hash) to the app's
  `crash/` directory. Secrets are redacted at the disk boundary.

## Known issues

| Area | Issue | Status |
|---|---|---|
| Models | The Models tab is under active rebuild (PIN storage client, NATIVE-R1-S3). "Deploy model" is not wired — the flow will be replaced by publish-a-model against the on-chain pinning registry, not patched. The on-chain model list shows placeholder data. | Rebuild scheduled (S3) |
| Learn | The Learn module ships as-is pending a rethink (parents'/teachers' accounts, missions — SPINE-S1). Education rosters/classroom data are placeholders. | Redesign scheduled |
| Wallet UX | Send/receive works but the flow is crypto-native; the dual-persona redesign (simple mode / power mode, receive-QR) moved to the S2.1 follow-on with its spec written first. | S2.1 |
| Chat | No file drag-and-drop into the conversation yet (Storage-tab drop works); stream formatting and multi-model turns are part of the same queued upgrade. | S2.1 |
| Agent Center | Functional but not self-explanatory; a first-run-persona redesign is queued (S2.1). | S2.1 |
| Compute | Hardware/provider cards can clip long machine descriptions; advanced per-setting configuration is queued (S2.1). | S2.1 |
| Settings | Layout restructure (own sidebar, identity & federation-connections section, deeper knowledge-graph options) queued (S2.1). | S2.1 |
| Dashboard | The "earned" figure is a label-level approximation until reward re-sourcing lands (VALIDATOR-S1/ECON-S1). Mempool size always shows 0; reward address plumbing (`get_reward_address()`) is stubbed. | Re-sourcing scheduled |
| Studio/CMO | The CMO integration group is stubbed; the Studio CMO hook is a no-op. Executor panels read stubs, not chain state. | Backlog |
| Build from source | Aliased-host cargo URLs may double-fetch `wallet-core` on some setups (see README). | Documented workaround |

## Fixed since the 2026-07-04 QA survey

- Session timeout mismatch (backend 1h vs UI 8h) — unified to a single
  shared constant.
- ComputeMarketplace address drift (3 failing tests pinning a pre-re-roll
  address) — tests now read the canonical generated address book, with a
  tripwire against new hardcoded literals.
- `citrate-network` pin lag — bumped; the stale pin computed a genesis
  that didn't match live chain 40204 and was the root cause of the
  bootnode disconnect.
- Account-abstraction vector pins recaptured from the live chain after
  the 2026-07-05 address re-roll.
