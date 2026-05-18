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

# citrate-gui-native

Slint-native desktop wallet, DAG explorer, and developer studio for the Citrate Network.

## Crates

| Path | Crate | Role |
|---|---|---|
| `gui/citrate_gui_native` | `citrate-gui-native` | Main Slint desktop application |
| `gui/citrate_desktop_app` | `citrate-desktop-app` | Backend service layer (chain client, mempool, RPC) |

## Cross-repo dependencies

This repo consumes chain + other Tier B repos via SSH git deps using **per-host SSH config aliases**:

| Alias | Source repo | Crates pulled |
|---|---|---|
| `github-citrate-chain` | `CitrateNetwork/citrate-chain` | wallet-core, storage, consensus, execution, sequencer, network, economics, api |
| `github-citrate-learning-center` | `CitrateNetwork/citrate-learning-center` | edu-app |
| `github-citrate-agent-runtime` | `CitrateNetwork/citrate-agent-runtime` | agent-legacy (imported as `citrate-agent-core`) |

Each alias maps to a separate read-only deploy key on the source repo. Local dev uses your personal GitHub SSH key (org membership grants access); CI uses the 3 deploy keys injected via `webfactory/ssh-agent`.

`.cargo/config.toml` sets `net.git-fetch-with-cli = true` so cargo uses the system git CLI (which honors SSH config aliases — libgit2 does not).

### CI secrets required

- `CHAIN_DEPLOY_KEY` — private half of the chain deploy key
- `LEARNING_CENTER_DEPLOY_KEY` — private half of the learning-center deploy key
- `AGENT_RUNTIME_DEPLOY_KEY` — private half of the agent-runtime deploy key

## Quick start

```bash
# First-time setup: ensure your personal SSH key is on GitHub and you
# have read access to the three sibling repos (org membership covers this).

cargo build --release
cargo run --release -p citrate-gui-native
```

## Repository context

Split from the Citrate monorepo on 2026-05-18 via `git filter-repo`, preserving 293 commits.

- **Monorepo archive**: https://github.com/CitrateNetwork/citrate-monorepo-archive
- **Agentile archive**: https://github.com/CitrateNetwork/citrate-agentile-archive
- **Chain**: https://github.com/CitrateNetwork/citrate-chain

## Known issues

The current Cargo URLs use aliased hosts (`github-citrate-chain` etc.) while some sibling Tier B repos use plain `github.com` in their own chain deps. Cargo treats these as different sources, so a chain crate like `citrate-wallet-core` may be fetched twice — once via each URL. This compiles but could surface as type-mismatch errors at API boundaries. Resolution: normalize all Tier B repos to use the same URL convention (planned for a Sprint 1 follow-up).

## License

[MIT](LICENSE).
