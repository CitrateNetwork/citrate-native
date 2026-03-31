# citrate-desktop-app

Headless application core for the Citrate desktop GUI -- typed services, view models, and event bus.

## Overview

Framework-agnostic orchestration layer that owns all application logic without depending
on any window toolkit (Slint, Tauri, egui). Provides typed service interfaces for every
domain (node, wallet, editor, git, terminal, chat, models, blocks, learning, compute),
an event bus for background-to-UI communication, and persistent application configuration.
The UI layer receives an `Arc<AppCore>` and calls typed methods directly -- no IPC, no JSON,
no string payloads.

## Modules

- `services` -- 12 typed service modules:
  - `node_service` -- Node lifecycle, embedded node start/stop, chain queries
  - `wallet_service` -- Account CRUD, balance, send transactions
  - `editor_service` -- Rope-based text buffer, syntax highlighting (syntect), undo/redo
  - `file_explorer_service` -- Directory tree with gitignore-aware traversal (walkdir + ignore)
  - `git_service` -- Status, diff, commit, branch, push/pull (libgit2)
  - `compiler_service` -- Solidity compiler integration (forge/solc)
  - `terminal_service` -- PTY sessions with ANSI parsing (vte + portable-pty)
  - `chat_service` -- AI chat via citrate RPC
  - `model_service` -- Model registry, deploy, inference
  - `block_service` -- Block/transaction explorer, DAG queries
  - `learning_service` -- Learning pools, staking, training
  - `compute_service` -- Compute marketplace, jobs, providers
- `event_bus` -- Typed `AppEvent` enum broadcast via `tokio::sync::broadcast`
- `view_models` -- UI view model definitions including IDE view models
- `ports` -- Port/interface abstractions
- `error` -- `AppError` enum

## Usage

```rust
use citrate_desktop_app::AppCore;
use std::sync::Arc;

let core = Arc::new(AppCore::new());       // loads config from disk
core.start().await?;                        // starts embedded node + wallet

let status = core.node.get_status().await;  // typed NodeStatus struct
let mut rx = core.events.subscribe();       // typed AppEvent stream
```

## Tests

```bash
cargo test -p citrate-desktop-app
```

Test count: 584 tests (420 unit + 27 service integration + 13 event bus + 20 editor +
10 file explorer + 11 compiler + 10 terminal + 52 wallet + 21 view models) covering
config serialization, service construction, event subscription, node start/stop,
wallet first-run detection, editor buffer operations, git operations, and terminal
ANSI parsing.
