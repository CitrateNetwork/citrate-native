# Citrate Native beta — 10-minute demo script

The brand-growth asset from the NATIVE-R1-S2 planset (WP-D4): a stranger
follows this on camera against a fresh install and hits **zero crashes,
zero dead buttons, zero placeholder data on the demo path**. Record the
run; the recording is the demoable asset.

Adjusted 2026-07-09 for the owner-approved beta descope: the receive-QR
scan and chat file-drop steps from the original demo definition moved to
S2.1 with the rest of Track B; this script only demonstrates what the
beta actually ships. Every step names what to say and what must be true
on screen.

**Setup:** clean machine (no dev toolchain), beta artifact from the
GitHub release, internet access. Delete any previous app data dir so
onboarding runs fresh.

---

## 1. Install (30s)

- Download the artifact for the machine, verify the `.sha256`, unpack,
  launch.
- **Say:** "Native app — Rust, no browser, no Electron. One binary."
- **Must be true:** app opens to onboarding, on-brand (citrate green,
  loader animation), no console window, no warnings.

## 2. Onboarding + wallet (90s)

- Walk the onboarding steps: create wallet, set password, confirm.
- **Say:** "Keys are generated locally and never leave the machine.
  The store is encrypted under the OS keychain."
- **Must be true:** every step advances; no jargon walls on the primary
  path; finishing lands on the dashboard.

## 3. Start the node (2min)

- Settings → Node Control → start (or the dashboard card).
- **Say:** "This is a real embedded node joining testnet-beta — encrypted
  transport to the bootnodes, and the local chain database is encrypted
  at rest by default. That's tested, not asserted: raw disk bytes are
  sealed ciphertext."
- **Must be true:** startup log/status shows encryption **on**; peer
  count reaches **≥2 bootnodes** (expect 4/4); block height starts
  advancing. Leave it syncing in the background.
- If anything dies here, the crash record in `crash/` is the story —
  but it shouldn't (20-cycle soak + 10-min live hold in the harness).

## 4. Faucet → balance (90s)

- Wallet screen → faucet claim.
- **Say:** "Test funds from the faucet — watch the balance."
- **Must be true:** claim lands and the balance updates from real chain
  state once synced; the transaction is visible.

## 5. Send + receive (60s)

- Show receive: copy the account address. Send a small amount to a
  second account (create one live, or use a prepared address).
- **Say:** "Send and receive work today; the friendlier dual-mode wallet
  UX is the next iteration — the list of what's coming is public in
  KNOWN_ISSUES."
- **Must be true:** the send confirms and both balances move.

## 6. Chat with the local model (90s)

- One chat turn with the bundled model.
- **Say:** "Local inference — the prompt never leaves the app."
- **Must be true:** response streams smoothly; no placeholder text.

## 7. DAG Explorer (60s)

- Open the DAG view while the node syncs; click one block; show its
  transactions.
- **Say:** "This is the app's own node's view of the DAG, live."
- **Must be true:** blocks render and advance; block detail shows real
  data (hashes de-emphasized per the design system).

## 8. Compute opt-in (30s)

- Toggle compute opt-in; show the hardware card.
- **Say:** "Opting hardware into the compute marketplace — granular
  controls are on the roadmap."
- **Must be true:** the toggle persists; the card shows this machine's
  real probe data.

## 9. Close honestly (30s)

- Open KNOWN_ISSUES.md from the About/README link.
- **Say:** "Everything that doesn't work yet is written down here —
  Models and Learn are mid-rebuild and say so. Nothing in this app
  pretends."

---

**Pass criteria for the recording:** every "must be true" held; no
retakes that hide a failure (a failed take = a bug to fix first);
total runtime ≤ 10 minutes.
