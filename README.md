# citrate-native

*Part of the **[Citrate Network](https://citrate.ai)** — own the means of computation. · [Docs](https://docs.citrate.ai) · [Run a node](https://citrate.ai/download) · [Contribute → free membership](https://github.com/CitrateNetwork/.github/blob/main/CONTRIBUTING.md)*

> The all-in-one native desktop app for the Citrate Network — wallet, embedded full node, DAG explorer, local AI chat, storage, and a developer studio in one Rust window.

## What it is

Citrate Native is a single Rust + [Slint](https://slint.dev) desktop application (no browser, no Electron, no WebKit) that bundles a crypto wallet, an **embedded full chain node**, a GhostDAG block explorer, a local AI chat client, an IPFS/Storage tab, and a developer studio/IDE. Keys never leave the machine (OS keychain, RocksDB chain data encrypted at rest), and it runs against the Citrate testnet-beta (chain id **40204**).

It connects out to a chain RPC + bootnodes for sync, to [citrate-identity](https://github.com/CitrateNetwork/citrate-identity) for OIDC login, and to an ERC-4337 bundler for account-abstraction transactions. Concept overview: https://docs.citrate.ai/apps.

## Prerequisites

Pure Cargo/Slint build — **no Node.js, no webkit2gtk** (Slint, not Tauri).

```bash
# Rust stable (pinned by rust-toolchain.toml) + rustfmt + clippy
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rustfmt clippy

# Linux system packages:
sudo apt-get install -y libclang-dev cmake libssl-dev pkg-config \
  libfontconfig1-dev libdbus-1-dev xvfb   # xvfb only needed for headless tests

# macOS (Intel + Apple Silicon) and Windows (x86_64) build with the system toolchain — no extra packages.
```

Building pulls a few Citrate crates over SSH (git, not crates.io) from `citrate-chain`, `citrate-learning-center`, and `citrate-agent-runtime`. `.cargo/config.toml` sets `net.git-fetch-with-cli = true`; you need SSH access (or the per-host aliases `github-citrate-chain`, `github-citrate-learning-center`, `github-citrate-agent-runtime`) configured for those repos.

Optional runtime services: an [Ollama](https://ollama.com) daemon for local chat, and an [IPFS/Kubo](https://docs.ipfs.tech/install/command-line/) daemon for the Storage tab.

## Build from source

```bash
git clone https://github.com/CitrateNetwork/citrate-native citrate-gui-native
cd citrate-gui-native

cargo build --release
# Artifact: target/release/citrate-native  (release profile: opt-level 3, LTO, codegen-units 1)

# Tests:
cargo test --workspace --locked
```

The workspace has three crates: `citrate-native` (the Slint app, default member), `citrate-desktop-app` (headless service layer — chain client, mempool, IDE, MCP host, keychain), and `citrate-ui-kit` (shared Slint components).

## Run locally

Native desktop app — it opens a window, it does not serve an HTTP port.

```bash
cargo run --release -p citrate-native
```

By default it runs against **testnet-beta** (`rpc.citrate.ai` + public bootnodes). To verify the embedded node against the live bootnodes:

```bash
cargo test --test live_bootnode_smoke -- --ignored --nocapture
```

The embedded node's own JSON-RPC listens on `127.0.0.1:8545` when you run in `devnet` mode (see below).

## Connect it locally

Native ships pointed at the public testnet, so it runs standalone. To wire it to a **local** stack on one machine:

1. **Local chain (chain 40204)** — run a devnet node from [citrate-chain](https://github.com/CitrateNetwork/citrate-chain), then point Native's RPC at it (or use the embedded node in `devnet` mode, which exposes `http://127.0.0.1:8545`):

   ```bash
   export CITRATE_RPC_URL=http://127.0.0.1:8545     # override the active RPC
   ```

   In `devnet` network mode Native runs its own embedded node on `DEFAULT_RPC_PORT=8545`; in `testnet` mode it uses `https://rpc.citrate.ai` and the public bootnodes.

2. **Identity / auth** — point OIDC at a locally-running [citrate-identity](https://github.com/CitrateNetwork/citrate-identity):

   ```bash
   export CITRATE_AUTH_URL=http://localhost:3000    # default: https://auth.citrate.ai (loopback http allowed)
   ```

3. **Bundler (ERC-4337 / account abstraction)** — point at a local [citrate-bundler](https://github.com/CitrateNetwork/citrate-bundler):

   ```bash
   export CITRATE_BUNDLER_URL=http://127.0.0.1:3010/rpc   # default: https://bundler.citrate.ai/rpc
   ```

4. **Local model + storage (optional)** — start Ollama (`http://localhost:11434`) for chat and Kubo (`http://127.0.0.1:5001`) for the Storage tab; both are auto-detected.

For the full multi-repo bring-up see `LOCAL_STACK.md` in [citrate-docs](https://github.com/CitrateNetwork/citrate-docs).

## Configuration

No `.env` file — config lives in a `config.json` under the OS data-local dir, with these env-var overrides:

| Variable | Default | Purpose |
|---|---|---|
| `CITRATE_RPC_URL` | derived (`devnet`→`:8545`, else `rpc.citrate.ai`) | chain JSON-RPC endpoint |
| `CITRATE_AUTH_URL` | `https://auth.citrate.ai` | identity/OIDC authority (https or loopback http) |
| `CITRATE_BUNDLER_URL` | `https://bundler.citrate.ai/rpc` | ERC-4337 bundler |
| `CITRATE_NODE_AGENT_ADDR` | `http://127.0.0.1:19600` | signing-relay node-agent address |
| `CITRATE_RELAY_ENABLED` | off | enable the signing relay (mirrors the Settings toggle) |
| `CITRATE_GUI_DATA_DIR` | OS data dir | override the app data directory |

Chain id `40204` is compiled in (`CHAIN_ID: u64 = 40204`). Secrets use the OS keychain (`keyring` crate, service `citrate-desktop`); the 32-byte node-storage master key is keyring-held and chain data / MCP tokens are AES-256-GCM encrypted at rest. Wallet sessions expire after one hour.

## Links

- Docs: https://docs.citrate.ai/apps
- Depends on: [citrate-chain](https://github.com/CitrateNetwork/citrate-chain) (RPC + node, chain 40204) · [citrate-identity](https://github.com/CitrateNetwork/citrate-identity) (OIDC) · [citrate-bundler](https://github.com/CitrateNetwork/citrate-bundler) (ERC-4337)
- Contributing (DCO): CONTRIBUTING.md · Security: SECURITY.md · License: LICENSE

## License

Source-available under the Business Source License 1.1 (see [`LICENSE`](LICENSE)); converts to Apache-2.0 on the Change Date stated in the license. This is the commercial application-layer / core tier of Citrate's open-core model; the infrastructure tier is Apache-2.0. Licensor: Citrate Inc.
