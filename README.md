---
created: 2026-05-18T05:50:00Z
branch: main
author: monorepo-split
status: active
split-from-monorepo-at: b3ccd5c7
split-from-monorepo-tag: pre-split-v0.4.0
archived-monorepo: https://github.com/CitrateNetwork/citrate-monorepo-archive
agentile-archive: https://github.com/CitrateNetwork/citrate-agentile-archive
---

# Citrate Native (beta)

The Citrate Network desktop app: a wallet, an embedded chain node, a DAG
explorer, local AI chat, and a developer studio in one native application
(Rust + Slint — no browser, no Electron).

This is a **public beta on testnet-beta (chain 40204)**. What works is
listed below; what doesn't yet is honestly listed in
[KNOWN_ISSUES.md](KNOWN_ISSUES.md). Nothing on screen pretends to work.

## Install

Grab the archive for your platform from the
[latest release](https://github.com/CitrateNetwork/citrate-native/releases):

| Platform | Artifact |
|---|---|
| macOS (Apple Silicon) | `citrate-native-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `citrate-native-x86_64-apple-darwin.tar.gz` |
| Linux (x86_64) | `citrate-native-x86_64-unknown-linux-gnu.tar.gz` |
| Windows (x86_64) | `citrate-native-x86_64-pc-windows-msvc.zip` |

Unpack and run the `citrate-native` binary — there is no installer and no
dev toolchain needed. Each artifact ships with a `.sha256` checksum.

## First run (what to expect)

1. **Onboarding** walks you through creating (or importing) a wallet and
   choosing a password. Your keys never leave the machine.
2. **Unlock** — sessions expire after 1 hour; the countdown you see is
   the same clock the backend enforces.
3. **Start the node** (Settings → Node Control, or the dashboard card).
   The embedded node connects to the four testnet bootnodes over
   encrypted (Noise) transport and starts syncing. Local chain data is
   **encrypted at rest by default** under a key in your OS keychain.
4. **Get test funds** — use the faucet from the wallet screen; the claim
   shows up in your balance once your node is synced.
5. **Chat** — talk to the bundled local model. Chat is local — prompts
   don't leave the app. (File drag-and-drop into chat is queued for the
   next iteration; the Storage tab accepts dropped files today.)
6. **Explore the DAG** — watch blocks land as your node syncs.

If the node ever crashes, the app writes a crash record (backtrace, last
log lines, build hash) to the app data `crash/` directory — attach it when
filing an issue.

## What this beta is for

Kicking the tires on a real network: run a node, hold and send test
tokens, chat with a local model, watch the DAG. The Models and Learn tabs
are visible but mid-rebuild — see [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for
the full honest list and what's scheduled where.

---

## Building from source (developers)

```bash
# First-time setup: ensure your personal SSH key is on GitHub and you
# have read access to the three sibling repos (org membership covers this).

cargo build --release
cargo run --release -p citrate-native
```

### Crates

| Path | Crate | Role |
|---|---|---|
| `gui/citrate_native` | `citrate-native` | Main Slint desktop application |
| `gui/citrate_desktop_app` | `citrate-desktop-app` | Backend service layer (chain client, mempool, RPC) |
| `gui/citrate_ui_kit` | `citrate-ui-kit` | Shared Slint design-system components |

### Cross-repo dependencies

This repo consumes chain + other Tier B repos via SSH git deps using **per-host SSH config aliases**:

| Alias | Source repo | Crates pulled |
|---|---|---|
| `github-citrate-chain` | `CitrateNetwork/citrate-chain` | wallet-core, storage, consensus, execution, sequencer, network, economics, api |
| `github-citrate-learning-center` | `CitrateNetwork/citrate-learning-center` | edu-app |
| `github-citrate-agent-runtime` | `CitrateNetwork/citrate-agent-runtime` | agent-legacy (imported as `citrate-agent-core`) |

Each alias maps to a separate read-only deploy key on the source repo. Local dev uses your personal GitHub SSH key (org membership grants access); CI uses the 3 deploy keys injected via `webfactory/ssh-agent`.

`.cargo/config.toml` sets `net.git-fetch-with-cli = true` so cargo uses the system git CLI (which honors SSH config aliases — libgit2 does not).

**Known source-build quirk:** the Cargo URLs use aliased hosts
(`github-citrate-chain` etc.) while some sibling repos use plain
`github.com` in their own chain deps. Cargo treats these as different
sources, so a chain crate like `citrate-wallet-core` may be fetched twice.
This compiles fine (slower first fetch); if you ever hit type-mismatch
errors at a chain API boundary, this is the first thing to check.
Normalizing the URL convention across Tier B repos is a planned follow-up.

### CI secrets required

- `CHAIN_DEPLOY_KEY` — private half of the chain deploy key
- `LEARNING_CENTER_DEPLOY_KEY` — private half of the learning-center deploy key
- `AGENT_RUNTIME_DEPLOY_KEY` — private half of the agent-runtime deploy key

### Testing

```bash
cargo test --workspace --locked          # full suite (CI-equivalent)
cargo test --test live_bootnode_smoke -- --ignored --nocapture
                                         # gated live smoke: 4/4 bootnodes + 10-min hold
```

## Repository context

Split from the Citrate monorepo on 2026-05-18 via `git filter-repo`, preserving 293 commits.

- **Monorepo archive**: https://github.com/CitrateNetwork/citrate-monorepo-archive
- **Agentile archive**: https://github.com/CitrateNetwork/citrate-agentile-archive
- **Chain**: https://github.com/CitrateNetwork/citrate-chain

## License

[MIT](LICENSE).
