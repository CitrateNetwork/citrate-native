# citrate-gui-native

The production Citrate desktop application -- Rust-native, built with Slint.

## Overview

Built with Slint (software renderer, no GPU required) and backed by `citrate-desktop-app`
for all business logic. The main binary creates a Tokio runtime, initializes `AppCore`,
and wires Slint callbacks to typed service methods. Async operations dispatch to Tokio
via `spawn_async`; results push back to the UI via `invoke_from_event_loop`.

## UI Modules (Slint)

The `ui/` directory contains Slint markup organized by feature:

- `shell/` -- Top-level layout, sidebar, status bar
- `onboarding/` -- First-run wallet creation flow
- `dashboard/` -- Overview stats and status
- `wallet/` -- Account list, send/receive, transaction history
- `chat/` -- AI chat interface
- `ide/` -- Code editor, file explorer, terminal
- `contracts/` -- Smart contract deployment and interaction
- `models/` -- AI model registry and inference
- `dag/` -- DAG visualization
- `learning/` -- Learning pools and training
- `compute/` -- Compute marketplace
- `settings/` -- Node and application settings
- `storage/` -- IPFS storage management
- `theme.slint` -- Design tokens (colors, spacing, typography)
- `app.slint` -- Root component

## Dependencies

- `citrate-desktop-app` -- Headless application core (all business logic)
- `citrate-wallet-core` -- SALT/wei conversion for send flow
- `citrate-storage` -- IPFS daemon for storage tab
- `slint` 1.9 -- UI framework (software renderer + winit backend)
- `arboard` -- Clipboard access for copy buttons

## Usage

```bash
# Run in development
cargo run -p citrate-gui-native

# Build release
cargo build -p citrate-gui-native --release
```

## Tests

```bash
cargo test -p citrate-gui-native
```

Test count: 83 tests (31 integration + 25 wallet E2E + 27 node lifecycle) covering Slint bindings and UI model construction.
