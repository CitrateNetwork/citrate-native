//! Citrate Desktop — Rust-native Slint GUI
#![allow(clippy::manual_is_multiple_of)]

use citrate_desktop_app::AppCore;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

slint::include_modules!();

/// EIP-55 mixed-case checksum encoding for Ethereum addresses.
/// Takes a hex address (with or without 0x prefix) and returns the checksummed form.
fn eip55_checksum(addr: &str) -> String {
    let addr_lower = addr.strip_prefix("0x").unwrap_or(addr).to_lowercase();
    let hash = {
        use sha3::{Digest, Keccak256};
        hex::encode(Keccak256::digest(addr_lower.as_bytes()))
    };
    let checksummed: String = addr_lower
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if c.is_ascii_alphabetic()
                && u8::from_str_radix(&hash[i..i + 1], 16).unwrap_or(0) >= 8
            {
                c.to_uppercase().next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    format!("0x{}", checksummed)
}

/// Strip markdown formatting for display in plain-text Slint Text elements.
/// Converts headers, bold markers, backticks, and list items to readable plain text.
fn clean_markdown(text: &str) -> String {
    let mut in_code_block = false;
    text.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Toggle fenced code blocks (``` markers)
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                return String::new();
            }
            // Inside code blocks, return content as-is (no markdown processing)
            if in_code_block {
                return format!("  {line}");
            }
            // Headers: remove # markers (check longest prefix first)
            let line = if let Some(rest) = trimmed.strip_prefix("#### ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("### ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("## ") {
                rest
            } else if let Some(rest) = trimmed.strip_prefix("# ") {
                rest
            } else {
                trimmed
            };
            // Bold: remove ** markers
            let line = line.replace("**", "");
            // Inline code: remove backtick markers
            let line = line.replace('`', "");
            // Unordered list items: convert - or * to bullet
            if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                format!("  \u{2022} {rest}")
            } else {
                line
            }
        })
        // Collapse consecutive blank lines into a single blank line
        .fold(Vec::<String>::new(), |mut acc, line| {
            if line.is_empty() {
                if acc.last().is_none_or(|l| !l.is_empty()) {
                    acc.push(line);
                }
            } else {
                acc.push(line);
            }
            acc
        })
        .join("\n")
        .trim()
        .to_string()
}

/// Shared atomic timestamp (unix epoch seconds) of last wallet unlock.
/// Set to 0 when locked. Background thread uses this to compute remaining session time.
static SESSION_UNLOCK_EPOCH: AtomicI64 = AtomicI64::new(0);

/// Session timeout duration in seconds (matches wallet_service::unlock which sets 3600).
const SESSION_TIMEOUT_SECS: i64 = 3600;

/// Push wallet accounts to the Slint UI as a VecModel.
/// Applies EIP-55 checksum encoding to all addresses for display.
fn push_accounts_to_ui(ui: &App, accounts: &[citrate_desktop_app::services::wallet_service::Account]) {
    let model_data: Vec<AccountData> = accounts
        .iter()
        .map(|a| AccountData {
            address: eip55_checksum(&a.address).into(),
            label: a.label.clone().into(),
            balance: a.balance.to_string().into(),
            is_default: a.is_default,
        })
        .collect();
    let model = std::rc::Rc::new(slint::VecModel::from(model_data));
    ui.set_wallet_accounts(model.into());
    ui.set_wallet_account_count(accounts.len() as i32);
}

/// Spawn an async task on the runtime without blocking the Slint event loop.
/// The callback receives a weak UI handle and runs on the tokio runtime.
/// Results are pushed back to the UI via `invoke_from_event_loop`.
fn spawn_async(
    rt: &tokio::runtime::Handle,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    rt.spawn(task);
}

/// IPFS daemon statistics fetched from the local HTTP API.
struct IpfsStats {
    peer_count: i32,
    pin_count: i32,
    repo_size: String,
}

/// Fetch peer count, pin count, and repo size from the running IPFS daemon.
/// Data source: IPFS kubo HTTP API at localhost:5001 (/api/v0/swarm/peers, /api/v0/pin/ls, /api/v0/repo/stat)
async fn ipfs_fetch_stats(client: &reqwest::Client) -> IpfsStats {
    let peer_count = match client
        .post("http://127.0.0.1:5001/api/v0/swarm/peers")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                json.get("Peers")
                    .and_then(|p| p.as_array())
                    .map(|a| a.len() as i32)
                    .unwrap_or(0)
            } else {
                0
            }
        }
        Err(_) => 0,
    };

    let pin_count = match client
        .post("http://127.0.0.1:5001/api/v0/pin/ls")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                json.get("Keys")
                    .and_then(|k| k.as_object())
                    .map(|m| m.len() as i32)
                    .unwrap_or(0)
            } else {
                0
            }
        }
        Err(_) => 0,
    };

    let repo_size = match client
        .post("http://127.0.0.1:5001/api/v0/repo/stat")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let bytes = json
                    .get("RepoSize")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                format_bytes(bytes)
            } else {
                "0 B".to_string()
            }
        }
        Err(_) => "0 B".to_string(),
    };

    IpfsStats {
        peer_count,
        pin_count,
        repo_size,
    }
}

/// Format a byte count into a human-readable string (B, KB, MB, GB, TB).
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,citrate=debug".into()),
        )
        .init();

    tracing::info!("Citrate Desktop starting (Slint native)");

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let app_core = Arc::new(AppCore::new());
    let is_first_run = rt.block_on(app_core.wallet.is_first_run());

    let ui = App::new().expect("Failed to create Slint window");

    // Initial state — derive environment label from loaded config, not hardcoded
    ui.set_show_onboarding(is_first_run);
    ui.set_active_tab("dashboard".into());
    {
        let config = rt.block_on(app_core.config.read());
        ui.set_environment(config.network.to_uppercase().into());
    }

    // Push bootnode and wallet data to UI
    {
        let config = rt.block_on(app_core.config.read());
        ui.set_bootnode_list(config.bootnodes.join("\n").into());

        // Push wallet accounts to UI (EIP-55 checksummed addresses)
        let accounts = rt.block_on(app_core.wallet.list_accounts());
        push_accounts_to_ui(&ui, &accounts);
        if let Some(first) = accounts.first() {
            ui.set_wallet_selected_address(eip55_checksum(&first.address).into());
            ui.set_wallet_selected_label(first.label.clone().into());
            ui.set_wallet_selected_balance(first.balance.to_string().into());
        }
    }

    // --- Create Wallet ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_create_wallet(move |password, _label| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let pwd = password.to_string();

        tracing::info!("Creating wallet...");
        spawn_async(&rt_h, async move {
            match core.wallet.create_wallet(&pwd).await {
                Ok(result) => {
                    tracing::info!("Wallet created: {}", result.address);
                    // Refresh account list after creation
                    let accounts = core.wallet.list_accounts().await;
                    let checksummed = eip55_checksum(&result.address);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_mnemonic(result.mnemonic.into());
                            ui.set_onboarding_wallet_address(checksummed.clone().into());
                            ui.set_onboarding_error("".into());
                            ui.set_onboarding_step(2);
                            ui.set_wallet_selected_address(checksummed.into());
                            ui.set_wallet_selected_label("Default".into());
                            push_accounts_to_ui(&ui, &accounts);
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Wallet creation failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Copy Mnemonic to Clipboard ---
    // NOTE: arboard::Clipboard::new() must run on the main thread.
    // Spawning a clipboard thread crashes accesskit on X11.
    let ui_w = ui.as_weak();
    ui.on_copy_mnemonic(move || {
        if let Some(ui) = ui_w.upgrade() {
            let mnemonic = ui.get_onboarding_mnemonic().to_string();
            if !mnemonic.is_empty() {
                match arboard::Clipboard::new() {
                    Ok(mut clipboard) => {
                        if clipboard.set_text(&mnemonic).is_ok() {
                            tracing::info!("Mnemonic copied to clipboard");
                        }
                    }
                    Err(e) => tracing::warn!("Clipboard not available: {}", e),
                }
            }
        }
    });

    // --- Verify Mnemonic Word ---
    let ui_w = ui.as_weak();
    ui.on_verify_mnemonic_word(move |input| {
        if let Some(ui) = ui_w.upgrade() {
            let mnemonic = ui.get_onboarding_mnemonic().to_string();
            let word_num = ui.get_onboarding_verify_word() as usize;
            let input_str = input.to_string().trim().to_lowercase();

            let words: Vec<&str> = mnemonic.split_whitespace().collect();
            if word_num > 0 && word_num <= words.len() {
                let expected = words[word_num - 1].to_lowercase();
                if input_str == expected {
                    tracing::info!("Mnemonic word {} verified correctly", word_num);
                    ui.set_onboarding_error("".into());
                    ui.set_onboarding_step(3);
                } else if input_str.is_empty() {
                    // Empty input = skip verification (for development testing)
                    tracing::info!("Mnemonic verification skipped (empty input)");
                    ui.set_onboarding_error("".into());
                    ui.set_onboarding_step(3);
                } else {
                    tracing::warn!("Mnemonic verification failed: expected word #{} '{}', got '{}'", word_num, expected, input_str);
                    ui.set_onboarding_error(
                        format!("Incorrect. Enter word #{} from your recovery phrase.", word_num).into()
                    );
                }
            }
        }
    });

    // Pick a random word number for verification (1-24)
    {
        let word_num = (rand::random::<u8>() % 24) as i32 + 1;
        ui.set_onboarding_verify_word(word_num);
    }

    // --- Start Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_start_node(move |env| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let environment = env.to_string();

        tracing::info!("Starting node for: {}", environment);

        if let Some(ui) = ui_w.upgrade() {
            ui.set_onboarding_node_status("Initializing storage...".into());
            ui.set_onboarding_node_progress(0.2);
        }

        spawn_async(&rt_h, async move {
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node started");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_node_status("Node running — connecting to network...".into());
                            ui.set_onboarding_node_progress(1.0);
                            ui.set_onboarding_node_ready(true);
                            ui.set_node_running(true);
                            ui.set_connection_status("Connecting to bootnode...".into());
                        }
                    });
                }
                Err(e) => {
                    let err = format!("Failed: {}", e);
                    tracing::error!("Node start failed: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_onboarding_node_status(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Check Model ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_check_model(move || {
        let ui_w = ui_w.clone();
        // Run detection in background — Ollama probe has a 2s timeout
        spawn_async(&rt_h, async move {
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let ready = detected.backend_type != "none";
            let status = if ready {
                format!("AI ready: {} — local-first, private", detected.display_name)
            } else {
                "No local model found — install Ollama or download a model in Settings".to_string()
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_onboarding_model_ready(ready);
                    ui.set_onboarding_model_status(status.into());
                }
            });
        });
    });

    // --- Onboarding Complete ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_onboarding_complete(move || {
        tracing::info!("Onboarding complete — landing on dashboard");
        let core = core.clone();
        let ui_w_inner = ui_w.clone();
        if let Some(ui) = ui_w.upgrade() {
            ui.set_show_onboarding(false);
            ui.set_active_tab("dashboard".into());
            // Auto-start node if not already running
            if !ui.get_node_running() {
                tracing::info!("Onboarding: auto-starting node");
                ui.set_connection_status("Starting node...".into());
                spawn_async(&rt_h, async move {
                    match core.node.start().await {
                        Ok(()) => {
                            tracing::info!("Onboarding: node auto-started");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w_inner.upgrade() {
                                    ui.set_node_running(true);
                                    ui.set_connection_status("Connecting to bootnode...".into());
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Onboarding: node auto-start failed: {}", e);
                            let err = format!("Node start failed: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w_inner.upgrade() {
                                    ui.set_connection_status(err.into());
                                }
                            });
                        }
                    }
                });
            }
        }
    });

    // --- Send Transaction ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_send(move |to, amount, password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let to_str = to.to_string();
        let amt_str = amount.to_string();
        let pwd_str = password.to_string();

        // Read the currently selected account address from UI state
        let from_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_else(|| "default".to_string());

        // Convert SALT amount → wei (1 SALT = 10^18 wei)
        // User types "1.5" meaning 1.5 SALT, backend expects wei string
        let wei_str = match citrate_wallet_core::format::salt_to_wei(&amt_str) {
            Ok(wei) => wei.to_string(),
            Err(e) => {
                let err_msg = format!("Invalid amount: {}", e);
                tracing::error!("{}", err_msg);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_send_error(err_msg.into());
                    }
                });
                return;
            }
        };

        tracing::info!("Sending {} SALT ({} wei) to {} from {}", amt_str, wei_str, to_str, from_addr);
        spawn_async(&rt_h, async move {
            match core.wallet.send_transaction(&from_addr, &to_str, &wei_str, &pwd_str).await {
                Ok(hash) => {
                    tracing::info!("Transaction sent: {}", hash);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_send_tx_hash(hash.into());
                            ui.set_send_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Send failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_send_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Send dialog close (reset state) ---
    let ui_w = ui.as_weak();
    ui.on_wallet_send_close(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_send_tx_hash("".into());
            ui.set_send_error("".into());
        }
    });

    // --- Unlock Wallet (lock screen) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_unlock_wallet(move |password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let pwd = password.to_string();

        // Use the selected account address (or first account if none selected)
        let selected_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();
        let addr = if selected_addr.is_empty() { "primary".to_string() } else { selected_addr };

        tracing::info!("Unlocking wallet for {}", addr);
        spawn_async(&rt_h, async move {
            match core.wallet.unlock(&addr, &pwd).await {
                Ok(_status) => {
                    tracing::info!("Wallet unlocked");
                    // Record unlock time for session countdown
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    SESSION_UNLOCK_EPOCH.store(now, Ordering::Relaxed);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_show_lock_screen(false);
                            ui.set_lock_error("".into());
                            ui.set_wallet_session_active(true);
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Unlock failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_lock_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Sign Out ---
    let ui_w = ui.as_weak();
    ui.on_sign_out(move || {
        tracing::info!("Signed out — showing onboarding");
        if let Some(ui) = ui_w.upgrade() {
            ui.set_show_lock_screen(false);
            ui.set_show_onboarding(true);
        }
    });

    // --- Tab Switching ---
    let ui_w = ui.as_weak();
    ui.on_tab_changed(move |tab| {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_active_tab(tab);
        }
    });

    // --- Background data push (non-blocking) ---
    // Runs on a separate OS thread but uses the MAIN runtime handle.
    // CRITICAL: Do NOT create a second tokio::Runtime — the EmbeddedNodeBackend
    // stores data behind tokio::sync::RwLock which is tied to the main runtime.
    // A second runtime causes cross-runtime deadlocks.
    {
        let ui_handle = ui.as_weak();
        let core = app_core.clone();
        let rt_handle = rt.handle().clone();
        std::thread::spawn(move || {
            let mut tick_counter: u32 = 0;
            // Track initial balance to compute earnings delta
            let mut baseline_balance_wei: Option<u128> = None;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                tick_counter += 1;

                // Refresh status from backend then read it
                rt_handle.block_on(core.node.refresh_status());
                let status = rt_handle.block_on(core.node.get_status());
                let conn_status = if status.peer_count > 0 {
                    format!("Connected ({} peers)", status.peer_count)
                } else if status.running {
                    "Connecting to bootnode...".to_string()
                } else {
                    "Disconnected".to_string()
                };

                // Get recent blocks from RocksDB for dashboard
                // Data source: citrate_storage::BlockStore via EmbeddedNodeBackend::get_block_summaries
                let recent_blocks = rt_handle.block_on(core.node.get_recent_blocks(3))
                    .unwrap_or_default();
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs()).unwrap_or(0);

                let block_data: Vec<(String, String, String)> = recent_blocks.iter().map(|b| {
                    let hash = if b.hash.len() > 18 {
                        format!("{}...{}", &b.hash[..10], &b.hash[b.hash.len()-4..])
                    } else {
                        b.hash.clone()
                    };
                    let txcount = format!("{} txn{}", b.tx_count, if b.tx_count == 1 { "" } else { "s" });
                    let age = if b.timestamp > 0 && now_secs > b.timestamp {
                        let secs = now_secs - b.timestamp;
                        if secs < 60 { format!("~{}s ago", secs) }
                        else if secs < 3600 { format!("~{}m ago", secs / 60) }
                        else { format!("~{}h ago", secs / 3600) }
                    } else if b.timestamp == 0 {
                        String::new()
                    } else {
                        "just now".to_string()
                    };
                    (hash, txcount, age)
                }).collect();

                let hash0 = block_data.first().map(|d| d.0.clone()).unwrap_or_default();
                let hash1 = block_data.get(1).map(|d| d.0.clone()).unwrap_or_default();
                let hash2 = block_data.get(2).map(|d| d.0.clone()).unwrap_or_default();
                let txc0 = block_data.first().map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let txc1 = block_data.get(1).map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let txc2 = block_data.get(2).map(|d| d.1.clone()).unwrap_or_else(|| "0 txns".into());
                let time0 = block_data.first().map(|d| d.2.clone()).unwrap_or_default();
                let time1 = block_data.get(1).map(|d| d.2.clone()).unwrap_or_default();
                let time2 = block_data.get(2).map(|d| d.2.clone()).unwrap_or_default();

                // Refresh wallet accounts every 5th tick (~15s)
                let wallet_accounts = if tick_counter.is_multiple_of(5) {
                    Some(rt_handle.block_on(core.wallet.list_accounts()))
                } else {
                    None
                };

                // H-02 FIX: Read the SELECTED address from UI state, not primary.
                // This prevents overwriting account #2's balance with account #1's.
                let selected_addr_for_balance = ui_handle.upgrade()
                    .map(|ui| ui.get_wallet_selected_address().to_string());

                // Get balance for the selected address (or primary as fallback)
                // Data source: StateDB account balance (wei) via NodeBackend::get_balance
                let (primary_balance, selected_balance) = rt_handle.block_on(async {
                    let primary = core.wallet.get_primary_address().await;

                    // Selected account balance
                    let sel_addr = selected_addr_for_balance
                        .as_deref()
                        .filter(|a| !a.is_empty())
                        .or(primary.as_deref());

                    let sel_bal = if let Some(addr) = sel_addr {
                        let wei_str = core.node.get_balance(addr).await.unwrap_or_else(|_| "0".to_string());
                        match wei_str.parse::<u128>() {
                            Ok(wei) => citrate_wallet_core::format::wei_to_salt(wei),
                            Err(_) => wei_str,
                        }
                    } else {
                        "0".to_string()
                    };

                    // Primary balance (for the header/overview widget)
                    let pri_bal = if let Some(addr) = &primary {
                        if Some(addr.as_str()) == sel_addr {
                            sel_bal.clone() // same address, reuse
                        } else {
                            let wei_str = core.node.get_balance(addr).await.unwrap_or_else(|_| "0".to_string());
                            match wei_str.parse::<u128>() {
                                Ok(wei) => citrate_wallet_core::format::wei_to_salt(wei),
                                Err(_) => wei_str,
                            }
                        }
                    } else {
                        "0".to_string()
                    };

                    (pri_bal, sel_bal)
                });

                // Calculate session remaining time from unlock epoch
                let (session_active, session_remaining) = {
                    let unlock_epoch = SESSION_UNLOCK_EPOCH.load(Ordering::Relaxed);
                    if unlock_epoch > 0 {
                        let elapsed = now_secs as i64 - unlock_epoch;
                        let remaining = SESSION_TIMEOUT_SECS - elapsed;
                        if remaining > 0 {
                            let mins = remaining / 60;
                            let secs = remaining % 60;
                            (true, format!("{}:{:02}", mins, secs))
                        } else {
                            // Session expired — clear it
                            SESSION_UNLOCK_EPOCH.store(0, Ordering::Relaxed);
                            (false, String::new())
                        }
                    } else {
                        (false, String::new())
                    }
                };

                // Compute earnings: delta between current balance and baseline
                // Use the raw wei from the balance query for accurate tracking
                let pri_addr = rt_handle.block_on(core.wallet.get_primary_address());
                let raw_wei = if let Some(addr) = &pri_addr {
                    rt_handle.block_on(core.node.get_balance(addr))
                        .ok()
                        .and_then(|s| s.parse::<u128>().ok())
                } else {
                    None
                };

                let earned_display = if let Some(wei) = raw_wei {
                    if baseline_balance_wei.is_none() && wei > 0 {
                        baseline_balance_wei = Some(wei);
                    }
                    if let Some(baseline) = baseline_balance_wei {
                        let earned_wei = wei.saturating_sub(baseline);
                        citrate_wallet_core::format::wei_to_salt(earned_wei)
                    } else {
                        "0.0000".to_string()
                    }
                } else {
                    "0.0000".to_string()
                };

                // Fetch tx history every 10th tick (~50s) to avoid storage churn
                let tx_list: Vec<TxData> = if tick_counter % 10 == 0 {
                    if let Some(ref addr) = pri_addr {
                        let txs = rt_handle.block_on(core.node.get_transactions_for_address(addr, 20));
                        txs.into_iter().map(|t| TxData {
                            hash: t.hash.into(),
                            tx_type: t.tx_type.into(),
                            amount: t.amount.into(),
                            counterparty: t.counterparty.into(),
                            status: t.status.into(),
                            timestamp: t.timestamp.into(),
                        }).collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };
                let has_tx_update = tick_counter % 10 == 0;

                let ui_handle = ui_handle.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        ui.set_node_running(status.running);
                        ui.set_block_height(status.block_height as i32);
                        ui.set_peer_count(status.peer_count as i32);
                        ui.set_mempool_size(status.mempool_size as i32);
                        ui.set_connection_status(conn_status.into());
                        ui.set_wallet_balance(primary_balance.into());
                        ui.set_wallet_selected_balance(selected_balance.into());
                        ui.set_salt_earned(earned_display.into());
                        // Push transaction history
                        if has_tx_update && !tx_list.is_empty() {
                            let model = std::rc::Rc::new(slint::VecModel::from(tx_list));
                            ui.set_wallet_transactions(model.into());
                        }
                        // Session timer
                        ui.set_wallet_session_active(session_active);
                        ui.set_wallet_session_remaining(session_remaining.into());
                        // Push wallet accounts if refreshed this tick
                        if let Some(ref accts) = wallet_accounts {
                            push_accounts_to_ui(&ui, accts);
                        }
                        // Block data from RocksDB
                        ui.set_block_hash_0(hash0.into());
                        ui.set_block_hash_1(hash1.into());
                        ui.set_block_hash_2(hash2.into());
                        ui.set_block_txcount_0(txc0.into());
                        ui.set_block_txcount_1(txc1.into());
                        ui.set_block_txcount_2(txc2.into());
                        ui.set_block_time_0(time0.into());
                        ui.set_block_time_1(time1.into());
                        ui.set_block_time_2(time2.into());
                    }
                });
            }
        });
    }

    // =========================================================================
    // IDE WIRING — File Explorer, Editor, Terminal, Git, Compiler
    // =========================================================================

    // --- IDE: Open file from explorer ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_ide_file_opened(move |path| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let path_str = path.to_string();
        let path_clone = path.clone();

        tracing::info!("IDE: opening file {}", path_str);
        spawn_async(&rt_h, async move {
            match core.editor.open_file(std::path::Path::new(&path_str)).await {
                Ok(buffer_id) => {
                    tracing::debug!("Opened buffer: {}", buffer_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_ide_selected_file(path_clone);
                        }
                    });
                }
                Err(e) => tracing::error!("Failed to open file: {}", e),
            }
        });
    });

    // --- IDE: Toggle directory in explorer ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_dir_toggled(move |path| {
        let core = core.clone();
        let path_str = path.to_string();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.file_explorer.toggle_directory(std::path::Path::new(&path_str)).await {
                tracing::error!("Failed to toggle directory: {}", e);
            }
        });
    });

    // --- IDE: Tab clicked ---
    ui.on_ide_tab_clicked(move |_tab_id| {
        // Active tab switch — editor state updates via callbacks
    });

    // --- IDE: Tab closed ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_tab_closed(move |tab_id| {
        let core = core.clone();
        let tab_str = tab_id.to_string();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.editor.close_buffer(&tab_str).await {
                tracing::error!("Failed to close tab: {}", e);
            }
        });
    });

    // --- IDE: Editor key pressed ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_editor_key(move |key| {
        let key_str = key.to_string();
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let buffers = core.editor.list_open_buffers().await;
            if let Some(active) = buffers.first() {
                let buf_id = active.id.clone();
                if key_str.len() == 1 && !key_str.is_empty() {
                    let content = core.editor.get_content(&buf_id).await
                        .unwrap_or_else(|_| String::new());
                    let byte_len = content.len();
                    let _ = core.editor.insert_text(&buf_id, byte_len, &key_str).await;
                }
            }
        });
    });

    // --- IDE: Terminal input ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_terminal_input(move |key| {
        let key_bytes = key.as_bytes().to_vec();
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let sessions = core.terminal.list_sessions().await;
            if let Some(active) = sessions.first() {
                let sid = active.session_id.clone();
                if let Err(e) = core.terminal.write_input(&sid, &key_bytes).await {
                    tracing::error!("Terminal input failed: {}", e);
                }
            }
        });
    });

    // --- IDE: Git stage ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_git_stage(move |path| {
        let core = core.clone();
        let path_str = path.to_string();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.git.stage(&[std::path::Path::new(&path_str)]).await {
                tracing::error!("Git stage failed: {}", e);
            }
        });
    });

    // --- IDE: Git unstage ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_git_unstage(move |path| {
        let core = core.clone();
        let path_str = path.to_string();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.git.unstage(&[std::path::Path::new(&path_str)]).await {
                tracing::error!("Git unstage failed: {}", e);
            }
        });
    });

    // --- IDE: Git commit ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_git_commit(move |message| {
        let core = core.clone();
        let msg = message.to_string();
        spawn_async(&rt_h, async move {
            match core.git.commit(&msg).await {
                Ok(hash) => tracing::info!("Committed: {}", hash),
                Err(e) => tracing::error!("Commit failed: {}", e),
            }
        });
    });

    // --- IDE: Git push ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_git_push(move || {
        let core = core.clone();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.git.push("origin").await {
                tracing::error!("Push failed: {}", e);
            }
        });
    });

    // --- IDE: Git pull ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_git_pull(move || {
        let core = core.clone();
        spawn_async(&rt_h, async move {
            if let Err(e) = core.git.pull("origin").await {
                tracing::error!("Pull failed: {}", e);
            }
        });
    });

    // --- IDE: Save file ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_ide_save_file(move || {
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let buffers = core.editor.list_open_buffers().await;
            if let Some(active) = buffers.first() {
                if let Err(e) = core.editor.save_buffer(&active.id).await {
                    tracing::error!("Save failed: {}", e);
                } else {
                    tracing::info!("Saved: {}", active.file_name);
                }
            }
        });
    });

    // --- IDE: Initialize file explorer with current directory ---
    {
        let core = app_core.clone();
        let rt_h = rt.handle().clone();
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Err(e) = rt_h.block_on(core.file_explorer.set_root(&cwd)) {
            tracing::warn!("Could not set IDE root to cwd: {}", e);
        } else {
            ui.set_ide_explorer_root(cwd.to_string_lossy().to_string().into());
        }
    }

    // --- Wallet: Create Account (from wallet view, not onboarding) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_create_account(move |password, label| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let pwd = password.to_string();
        let lbl = label.to_string();
        tracing::info!("Wallet: creating new account '{}'", lbl);
        spawn_async(&rt_h, async move {
            match core.wallet.create_wallet(&pwd).await {
                Ok(result) => {
                    tracing::info!("New account created: {}", result.address);
                    let addr = eip55_checksum(&result.address);
                    let accounts = core.wallet.list_accounts().await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_selected_address(addr.into());
                            ui.set_wallet_selected_label(lbl.into());
                            push_accounts_to_ui(&ui, &accounts);
                            // Close the create dialog on success
                            ui.set_wallet_create_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Create account failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_create_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Import from Mnemonic ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_import_mnemonic(move |mnemonic, password| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let mnemonic_str = mnemonic.to_string();
        let pwd = password.to_string();
        tracing::info!("Wallet: importing from mnemonic");
        spawn_async(&rt_h, async move {
            // Data source: citrate_wallet_core::KeyManager::recover_from_mnemonic
            match core.wallet.import_from_mnemonic(&mnemonic_str, &pwd).await {
                Ok(result) => {
                    tracing::info!("Imported account: {}", result.address);
                    let addr = eip55_checksum(&result.address);
                    let accounts = core.wallet.list_accounts().await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_selected_address(addr.into());
                            ui.set_wallet_selected_label("Imported Account".into());
                            push_accounts_to_ui(&ui, &accounts);
                            ui.set_wallet_import_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::error!("Import failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_import_error(err.into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Select Account ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_select_account(move |index| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let idx = index as usize;
        tracing::info!("Wallet: selecting account at index {}", idx);
        spawn_async(&rt_h, async move {
            let accounts = core.wallet.list_accounts().await;
            if let Some(acct) = accounts.get(idx) {
                let addr = eip55_checksum(&acct.address);
                let label = acct.label.clone();
                let balance = acct.balance.clone();
                tracing::info!("Selected account: {} ({})", label, addr);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_selected_address(addr.into());
                        ui.set_wallet_selected_label(label.into());
                        ui.set_wallet_selected_balance(balance.into());
                    }
                });
            }
        });
    });

    // --- Wallet: Export Private Key (requires re-auth via password) ---
    // Data source: citrate_wallet_core::KeyManager::export_private_key
    // Decrypts the on-disk keystore entry with the provided password and returns hex.
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_export_key(move |password| {
        let ui_w = ui_w.clone();
        let pwd = password.to_string();

        // Clear any previously exported key state (security: don't leave keys in memory)
        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_exported_key("".into());
            ui.set_wallet_export_error("".into());
        }

        // Read the currently selected account address from UI state
        let selected_addr = ui_w.upgrade()
            .map(|ui| ui.get_wallet_selected_address().to_string())
            .unwrap_or_default();

        if selected_addr.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_export_error("No account selected".into());
                }
            });
            return;
        }

        tracing::info!("Wallet: exporting private key for {} (re-auth required)", selected_addr);
        spawn_async(&rt_h, async move {
            // Create a fresh KeyManager pointing at the same keystore to read the encrypted entry.
            // This is safe: export_private_key re-decrypts from disk, independent of unlock state.
            let config = citrate_wallet_core::WalletConfig::default();
            let keystore_path = std::path::PathBuf::from(&config.keystore_path);
            let km = citrate_wallet_core::KeyManager::new(&keystore_path);

            if let Err(e) = km.load() {
                let err = format!("Failed to load keystore: {}", e);
                tracing::error!("{}", err);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_export_error(err.into());
                    }
                });
                return;
            }

            match km.export_private_key(&selected_addr, &pwd) {
                Ok(hex_key) => {
                    tracing::info!("Private key exported for {} (length: {} hex chars)", selected_addr, hex_key.len());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_exported_key(hex_key.into());
                            ui.set_wallet_export_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let err = format!("{}", e);
                    tracing::error!("Export failed: {}", err);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_export_error(err.into());
                            ui.set_wallet_exported_key("".into());
                        }
                    });
                }
            }
        });
    });

    // --- Wallet: Copy Exported Key to Clipboard ---
    let ui_w = ui.as_weak();
    ui.on_wallet_copy_exported_key(move || {
        if let Some(ui) = ui_w.upgrade() {
            let key = ui.get_wallet_exported_key().to_string();
            if !key.is_empty() {
                match arboard::Clipboard::new() {
                    Ok(mut clipboard) => {
                        if let Err(e) = clipboard.set_text(&key) {
                            tracing::error!("Failed to copy to clipboard: {}", e);
                        } else {
                            tracing::info!("Exported key copied to clipboard");
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to access clipboard: {}", e);
                    }
                }
            }
        }
    });

    // --- Faucet request ---
    // Data source: POST https://faucet.citrate.ai/faucet with {"address": "0x..."}
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let core = app_core.clone();
    ui.on_wallet_request_faucet(move || {
        let ui_w = ui_w.clone();
        let core = core.clone();
        tracing::info!("Wallet: requesting SALT from faucet");

        if let Some(ui) = ui_w.upgrade() {
            ui.set_wallet_faucet_status("Requesting...".into());
        }

        spawn_async(&rt_h, async move {
            let address = core.wallet.get_primary_address().await
                .unwrap_or_default();
            if address.is_empty() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_wallet_faucet_status("No wallet".into());
                    }
                });
                return;
            }

            let client = reqwest::Client::new();
            let body = serde_json::json!({"address": address});
            match client.post("https://faucet.citrate.ai/faucet")
                .json(&body)
                .timeout(std::time::Duration::from_secs(10))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Faucet: SALT requested for {}", address);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status("Sent!".into());
                        }
                    });
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();
                    tracing::warn!("Faucet: {} — {}", status, body_text);
                    let msg = if body_text.contains("rate") || body_text.contains("limit") {
                        "Rate limited".to_string()
                    } else {
                        format!("Error {}", status.as_u16())
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status(msg.into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Faucet: request failed — {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_wallet_faucet_status("Failed".into());
                        }
                    });
                }
            }
        });
    });

    // --- IDE: Pop out to standalone window ---
    ui.on_ide_pop_out(move || {
        tracing::info!("IDE: pop-out requested — launching standalone IDE window");
        // Slint doesn't support multiple windows from the same process natively.
        // The production approach: launch a second process with the IDE as the root component.
        // For now, log the request — this will be wired when we have a standalone IDE binary.
    });

    // Terminal and git init deferred — will initialize on first tab switch to Studio.
    // This prevents blocking the UI at startup.

    // IDE state updates happen through callbacks (file open, edit, etc.)
    // No polling timer — prevents blocking the Slint event loop.

    // Terminal updates will use slint::invoke_from_event_loop from a background thread.
    // No polling timer — prevents blocking the Slint event loop.

    // =========================================================================
    // CHAT WIRING — AI agent via citrate_chatCompletion RPC
    // =========================================================================

    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_chat_send(move |message| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        let msg = message.to_string();

        // Show thinking state and user's message immediately (doesn't block)
        if let Some(ui) = ui_w.upgrade() {
            ui.set_chat_thinking(true);
            ui.set_chat_error("".into());
            // Push user message to bubbles immediately so they see it before AI responds
            let current = ui.get_chat_messages();
            let mut msgs: Vec<ChatMessageData> = {
                use slint::Model;
                (0..current.row_count())
                    .filter_map(|i| current.row_data(i))
                    .collect()
            };
            msgs.push(ChatMessageData {
                role: "user".into(),
                content: msg.clone().into(),
            });
            let model = std::rc::Rc::new(slint::VecModel::from(msgs));
            ui.set_chat_messages(model.into());
        }

        // Send async — never block the UI thread
        spawn_async(&rt_h, async move {
            match core.chat.send_message(&msg).await {
                Ok(response) => {
                    let content = clean_markdown(&response.content);
                    // Build structured message list for individual bubbles
                    let messages = core.chat.get_messages().await;
                    let slint_messages: Vec<ChatMessageData> = messages.iter()
                        .filter(|m| m.role == "user" || m.role == "assistant")
                        .map(|m| {
                            let cleaned = if m.role == "user" {
                                m.content.clone()
                            } else {
                                clean_markdown(&m.content)
                            };
                            ChatMessageData {
                                role: m.role.clone().into(),
                                content: cleaned.into(),
                            }
                        })
                        .collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_last_response(content.into());
                            let model = std::rc::Rc::new(slint::VecModel::from(slint_messages));
                            ui.set_chat_messages(model.into());
                            ui.set_chat_thinking(false);
                            ui.set_chat_model_loaded(true);
                        }
                    });
                }
                Err(e) => {
                    let raw = e.to_string();
                    let err_msg = if raw.contains("not found") || raw.contains("No model") {
                        "No AI model loaded. Go to Settings > AI Configuration to configure a provider or download a local model.".to_string()
                    } else {
                        raw.clone()
                    };
                    tracing::error!("Chat error: {}", raw);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_error(err_msg.into());
                            ui.set_chat_thinking(false);
                        }
                    });
                }
            }
        });
    });

    // Set initial chat context with wallet info and auto-detect AI backend.
    // Runs in a background thread to avoid blocking the UI — Ollama detection
    // has a 2-second timeout that would freeze the window on startup.
    {
        let core = app_core.clone();
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        let config_network = rt_h.block_on(async {
            core.config.read().await.network.clone()
        });
        spawn_async(&rt_h, async move {
            // Set chat context with real wallet state
            let address = core.wallet.get_primary_address().await
                .unwrap_or_else(|| "not connected".to_string());
            let balance = core.node.get_balance(&address).await
                .unwrap_or_else(|_| "0".to_string());
            let height = core.node.get_status().await.block_height;
            core.chat.set_context(&address, &balance, &config_network, height).await;

            // Auto-detect local AI backend: Ollama (preferred) → local GGUF → none
            // This is local-first: only localhost:11434 and filesystem are checked.
            // No external API calls are made during detection.
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let model_loaded = matches!(detected.backend_type.as_str(), "ollama" | "gguf");
            let display_name = detected.display_name.clone();
            let model_id = detected.model_id.clone();
            let backend_type = detected.backend_type.clone();

            if model_loaded {
                tracing::info!("Auto-detected AI backend: {} ({})", display_name, backend_type);
                core.chat.set_model(&model_id).await;
            } else {
                tracing::info!("No local AI backend detected. User can configure one in Settings.");
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_chat_model_loaded(model_loaded);
                    ui.set_chat_model_name(display_name.into());
                }
            });
        });
    }

    // =========================================================================
    // SETTINGS WIRING — Node control, bootnodes, theme, factory reset
    // =========================================================================

    // --- Settings: Start Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_start_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: starting node");
        spawn_async(&rt_h, async move {
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node started via settings");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() { ui.set_node_running(true); }
                    });
                }
                Err(e) => tracing::error!("Node start failed: {}", e),
            }
        });
    });

    // --- Settings: Stop Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_stop_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: stopping node");
        spawn_async(&rt_h, async move {
            match core.node.stop().await {
                Ok(()) => {
                    tracing::info!("Node stopped via settings");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() { ui.set_node_running(false); }
                    });
                }
                Err(e) => tracing::error!("Node stop failed: {}", e),
            }
        });
    });

    // --- Settings: Restart Node ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_restart_node(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: restarting node");
        let ui_w2 = ui_w.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w2.upgrade() {
                ui.set_connection_status("Restarting...".into());
            }
        });
        spawn_async(&rt_h, async move {
            let _ = core.node.stop().await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match core.node.start().await {
                Ok(()) => {
                    tracing::info!("Node restarted");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_node_running(true);
                            ui.set_connection_status("Connecting to bootnode...".into());
                        }
                    });
                }
                Err(e) => {
                    let msg = format!("Restart failed: {}", e);
                    tracing::error!("{}", msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_connection_status(msg.into());
                        }
                    });
                }
            }
        });
    });

    // --- Settings: Add Bootnode ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_add_bootnode(move |addr| {
        let addr_str = addr.to_string();
        tracing::info!("Settings: adding bootnode {}", addr_str);
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            if !config.bootnodes.contains(&addr_str) {
                config.bootnodes.push(addr_str);
                let _ = config.save();
            }
        });
    });

    // --- Settings: Set Theme ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_set_theme(move |mode| {
        let mode_str = mode.to_string();
        tracing::info!("Settings: theme = {}", mode_str);
        let core = core.clone();
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.theme = mode_str;
            let _ = config.save();
        });
    });

    // --- Settings: Factory Reset ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_factory_reset(move || {
        tracing::warn!("Settings: factory reset requested");
        let core = core.clone();
        let ui_w = ui_w.clone();
        spawn_async(&rt_h, async move {
            let data_dir = core.config.read().await.data_dir.clone();
            let _ = core.node.stop().await;
            if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                tracing::error!("Factory reset: failed to remove {}: {}", data_dir, e);
            } else {
                tracing::info!("Factory reset: removed {}", data_dir);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_show_onboarding(true);
                    ui.set_node_running(false);
                    ui.set_onboarding_step(0);
                }
            });
        });
    });

    // =========================================================================
    // WALLET DETAIL WIRING — Copy address, lock, unlock
    // =========================================================================

    // --- Wallet: Copy Address ---
    let ui_w = ui.as_weak();
    ui.on_wallet_copy_address(move || {
        if let Some(ui) = ui_w.upgrade() {
            let address = ui.get_wallet_selected_address().to_string();
            if !address.is_empty() {
                match arboard::Clipboard::new() {
                    Ok(mut clipboard) => {
                        if clipboard.set_text(&address).is_ok() {
                            tracing::info!("Address copied to clipboard");
                        }
                    }
                    Err(e) => tracing::warn!("Clipboard not available: {}", e),
                }
            }
        }
    });

    // --- Wallet: Lock ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_lock_clicked(move || {
        tracing::info!("Wallet: locking");
        let core = core.clone();
        let ui_w = ui_w.clone();
        SESSION_UNLOCK_EPOCH.store(0, Ordering::Relaxed);
        spawn_async(&rt_h, async move {
            if let Err(e) = core.wallet.lock().await {
                tracing::error!("Wallet lock failed: {}", e);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_wallet_session_active(false);
                    ui.set_wallet_session_remaining("".into());
                    ui.set_show_lock_screen(true);
                }
            });
        });
    });

    // --- Wallet: Unlock (from wallet view, not lock screen) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_wallet_unlock_clicked(move || {
        // The wallet view unlock just flags that the session needs re-auth.
        // The lock screen will handle the actual password entry.
        tracing::info!("Wallet: unlock requested, showing lock screen");
        if let Some(ui) = ui_w.upgrade() {
            ui.set_show_lock_screen(true);
        }
        let _ = (&core, &rt_h); // used for future re-auth flow
    });

    // =========================================================================
    // DAG EXPLORER WIRING — Search, select block, back
    // =========================================================================

    // --- DAG: Search by height or hash ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_dag_search(move |query| {
        let query_str = query.to_string().trim().to_string();
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("DAG: searching for '{}'", query_str);

        if let Ok(height) = query_str.parse::<u64>() {
            // Read block directly from local RocksDB via NodeService
            spawn_async(&rt_h, async move {
                let summaries = core.node.get_recent_blocks(50).await
                    .unwrap_or_default();
                let block = summaries.iter().find(|b| b.height == height);
                match block {
                    Some(block) => {
                        tracing::info!("DAG: found block at height {}", block.height);
                        let hash = block.hash.clone();
                        let h = block.height as i32;
                        let ts = block.timestamp.to_string();
                        let tx = block.tx_count as i32;
                        let parent = block.selected_parent.clone();
                        let bs = block.blue_score as i32;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_dag_show_detail(true);
                                ui.set_dag_detail_hash(hash.into());
                                ui.set_dag_detail_height(h);
                                ui.set_dag_detail_timestamp(ts.into());
                                ui.set_dag_detail_tx_count(tx);
                                ui.set_dag_detail_parent(parent.into());
                                ui.set_dag_detail_blue_score(bs);
                                ui.set_dag_detail_proposer("".into());
                            }
                        });
                    }
                    None => tracing::warn!("DAG: block {} not in local storage", height),
                }
            });
        }
    });

    // --- DAG: Select block (click on block row) ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_dag_select_block(move |height| {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("DAG: selecting block at height {}", height);

        // Read block detail directly from local RocksDB storage via NodeService
        // (not via HTTP RPC which requires a running JSON-RPC server)
        spawn_async(&rt_h, async move {
            let all_summaries = core.node.get_recent_blocks(50).await
                .unwrap_or_default();
            let block = all_summaries.iter().find(|b| b.height == height as u64);

            match block {
                Some(block) => {
                    let hash = block.hash.clone();
                    let h = block.height as i32;
                    let ts = block.timestamp.to_string();
                    let tx = block.tx_count as i32;
                    let parent = block.selected_parent.clone();
                    let bs = block.blue_score as i32;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_dag_show_detail(true);
                            ui.set_dag_detail_hash(hash.into());
                            ui.set_dag_detail_height(h);
                            ui.set_dag_detail_timestamp(ts.into());
                            ui.set_dag_detail_tx_count(tx);
                            ui.set_dag_detail_parent(parent.into());
                            ui.set_dag_detail_blue_score(bs);
                            ui.set_dag_detail_proposer("".into());
                        }
                    });
                }
                None => {
                    tracing::warn!("DAG: block at height {} not found in local storage", height);
                }
            }
        });
    });

    // --- DAG: Back to list ---
    let ui_w = ui.as_weak();
    ui.on_dag_back_to_list(move || {
        if let Some(ui) = ui_w.upgrade() {
            ui.set_dag_show_detail(false);
        }
    });

    // =========================================================================
    // MODELS WIRING — Load, deploy, inference, browse
    // =========================================================================

    // --- Models: Load model ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_load_model(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: loading model");
        spawn_async(&rt_h, async move {
            match core.models.refresh_models().await {
                Ok(models) => {
                    tracing::info!("Models: loaded {} models", models.len());
                    let count = models.len() as i32;
                    let first_name = models.first().map(|m| m.name.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_count(count);
                            if let Some(name) = first_name {
                                ui.set_chat_model_name(name.into());
                                ui.set_chat_model_loaded(true);
                            }
                        }
                    });
                }
                Err(e) => tracing::error!("Models: load failed: {}", e),
            }
        });
    });

    // --- Models: Deploy model to precompile ---
    // Data source: tx to model precompile at 0x0000000000000000000000000000000000001000
    // Method: registerModel(bytes32 modelHash, string ipfsCid)
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_deploy_model(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: deploying model to on-chain registry");

        spawn_async(&rt_h, async move {
            // 1. Find local model file
            let model_dir = dirs::data_local_dir()
                .map(|d| d.join("citrate").join("models"));
            let model_path = match model_dir {
                Some(dir) if dir.exists() => {
                    std::fs::read_dir(&dir).ok()
                        .and_then(|entries| entries
                            .filter_map(|e| e.ok())
                            .find(|e| e.path().extension().is_some_and(|ext| ext == "gguf"))
                            .map(|e| e.path()))
                }
                _ => None,
            };
            let path = match model_path {
                Some(p) => p,
                None => {
                    tracing::warn!("Models: no local GGUF file found to deploy");
                    return;
                }
            };

            // 2. Compute model hash (SHA3 of filename for deterministic ID)
            let filename = path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "model.gguf".to_string());
            let model_hash: [u8; 32] = {
                use sha3::{Digest, Keccak256};
                let mut hasher = Keccak256::new();
                hasher.update(filename.as_bytes());
                hasher.finalize().into()
            };

            // 3. Get IPFS CID if pinned, otherwise use filename
            let cid = ui_w.upgrade()
                .map(|ui| ui.get_models_ipfs_cid().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| filename.clone());

            // 4. Build registerModel(bytes32,string) calldata
            // Selector: keccak256("registerModel(bytes32,string)")[:4]
            let selector: [u8; 4] = {
                use sha3::{Digest, Keccak256};
                let hash = Keccak256::digest(b"registerModel(bytes32,string)");
                [hash[0], hash[1], hash[2], hash[3]]
            };

            // ABI encode: model_hash (32 bytes) + offset to string (32 bytes) + string length (32 bytes) + string data
            let cid_bytes = cid.as_bytes();
            let padded_len = cid_bytes.len().div_ceil(32) * 32;
            let mut calldata = Vec::with_capacity(4 + 32 + 32 + 32 + padded_len);
            calldata.extend_from_slice(&selector);
            calldata.extend_from_slice(&model_hash);
            // Offset to string data (64 bytes from start of args = 0x40)
            let mut offset = [0u8; 32];
            offset[31] = 0x40;
            calldata.extend_from_slice(&offset);
            // String length
            let mut len_bytes = [0u8; 32];
            len_bytes[31] = cid_bytes.len() as u8;
            calldata.extend_from_slice(&len_bytes);
            // String data (right-padded to 32 bytes)
            calldata.extend_from_slice(cid_bytes);
            calldata.resize(calldata.len() + padded_len - cid_bytes.len(), 0);

            // 5. Get sender address
            let from = match core.wallet.get_primary_address().await {
                Some(addr) => addr,
                None => {
                    tracing::error!("Models: no wallet address for deploy tx");
                    return;
                }
            };

            // 6. Send tx to model precompile
            let precompile = "0x0000000000000000000000000000000000001000";
            tracing::info!("Models: sending registerModel tx to {} (hash={}, cid={})",
                precompile, hex::encode(&model_hash[..8]), cid);

            match core.wallet.send_transaction_with_data(
                &from, precompile, "0", calldata, ""
            ).await {
                Ok(tx_hash) => {
                    tracing::info!("Models: deploy tx submitted: {}", tx_hash);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_ipfs_cid(format!("Deployed: {}", tx_hash).into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Models: deploy failed: {}", e);
                }
            }
        });
    });

    // --- Models: Run inference ---
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_run_inference(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: running inference test");
        spawn_async(&rt_h, async move {
            match core.chat.send_message("Hello, this is an inference test.").await {
                Ok(response) => {
                    let content = clean_markdown(&response.content);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_chat_last_response(content.into());
                        }
                    });
                }
                Err(e) => tracing::error!("Models: inference test failed: {}", e),
            }
        });
    });

    // --- Models: Browse HuggingFace ---
    ui.on_models_browse_huggingface(move || {
        tracing::info!("Models: opening HuggingFace in browser");
        // Open URL in default browser via xdg-open (Linux)
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://huggingface.co/models?search=gguf")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Models: Pin local model to IPFS ---
    // Data source: IPFS daemon HTTP API at localhost:5001/api/v0/add
    // Reads local GGUF file from ~/.local/share/citrate/models/ and adds to IPFS
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_models_pin_to_ipfs(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Models: pinning local model to IPFS");

        // Show pinning state immediately
        if let Some(ui) = ui_w.upgrade() {
            ui.set_models_pinning(true);
        }

        spawn_async(&rt_h, async move {
            // Find the local model file
            let model_dir = dirs::data_local_dir()
                .map(|d| d.join("citrate").join("models"));
            let model_path = match model_dir {
                Some(dir) if dir.exists() => {
                    match std::fs::read_dir(&dir) {
                        Ok(entries) => entries
                            .filter_map(|e| e.ok())
                            .find(|e| e.path().extension().is_some_and(|ext| ext == "gguf"))
                            .map(|e| e.path()),
                        Err(_) => None,
                    }
                }
                _ => None,
            };

            let path = match model_path {
                Some(p) => p,
                None => {
                    tracing::warn!("Models: no local GGUF file found to pin");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                    return;
                }
            };

            tracing::info!("Models: adding {} to IPFS via CLI", path.display());

            // Use `ipfs add --pin` CLI — simpler than multipart HTTP and handles large files
            let path_str = path.to_string_lossy().to_string();
            match tokio::process::Command::new("ipfs")
                .args(["add", "--pin", "--quieter", &path_str])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let cid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    tracing::info!("Models: pinned to IPFS — CID: {}", cid);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_ipfs_cid(cid.into());
                            ui.set_models_pinning(false);
                        }
                    });
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!("Models: ipfs add failed — {}", stderr);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Models: ipfs command not found or failed — {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_models_pinning(false);
                        }
                    });
                }
            }
        });
    });

    // --- Settings: Connect GitHub (open token settings in browser) ---
    ui.on_settings_connect_github(move || {
        tracing::info!("Settings: opening GitHub token settings in browser");
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://github.com/settings/tokens")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Settings: Connect HuggingFace (open token settings in browser) ---
    ui.on_settings_connect_huggingface(move || {
        tracing::info!("Settings: opening HuggingFace token settings in browser");
        if let Err(e) = std::process::Command::new("xdg-open")
            .arg("https://huggingface.co/settings/tokens")
            .spawn()
        {
            tracing::warn!("Could not open browser: {}", e);
        }
    });

    // --- Settings: Save Integration Token ---
    // Data source: AppConfig.integration_tokens persisted to disk via config.save()
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_save_integration_token(move |name, token| {
        let name_str = name.to_string();
        let token_str = token.to_string();
        let core = core.clone();
        tracing::info!("Settings: saving integration token for {} ({} chars)", name_str, token_str.len());
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.integration_tokens.insert(name_str.clone(), token_str);
            if let Err(e) = config.save() {
                tracing::error!("Failed to save config: {}", e);
            } else {
                tracing::info!("Settings: {} integration token saved to disk", name_str);
            }
        });
    });

    // --- Settings: Set Environment (network switch) ---
    // Data source: AppConfig.network persisted to disk via config.save()
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_set_environment(move |env| {
        let env_str = env.to_string();
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Settings: switching environment to {}", env_str);
        spawn_async(&rt_h, async move {
            // Stop current node — waits for background tasks and releases RocksDB
            if let Err(e) = core.node.stop().await {
                tracing::warn!("Stop during env switch: {}", e);
            }

            // Reconfigure bootnodes based on environment
            let lower = env_str.to_lowercase();
            match lower.as_str() {
                "devnet" => {
                    core.node.update_bootnodes(vec![]).await;
                    tracing::info!("Devnet: no bootnodes (local only)");
                }
                "testnet" => {
                    core.node.update_bootnodes(vec!["159.65.227.42:30303".to_string()]).await;
                    tracing::info!("Testnet: connecting to bootnode");
                }
                _ => {}
            }

            // Update config: network, chain_id, data_dir, and bootnodes move together
            let mut config = core.config.write().await;
            config.network = lower.clone();
            config.chain_id = citrate_desktop_app::chain_id_for_network(&lower);
            config.data_dir = citrate_desktop_app::data_dir_for_network(&lower);
            config.bootnodes = match lower.as_str() {
                "devnet" => vec![],
                _ => vec!["159.65.227.42:30303".to_string()],
            };
            tracing::info!(
                "Environment switch: network={}, chain_id={}, data_dir={}",
                lower, config.chain_id, config.data_dir
            );
            if let Err(e) = config.save() {
                tracing::error!("Failed to save config: {}", e);
            }
            drop(config);

            // Restart node with new config (reads updated data_dir + chain_id)
            if let Err(e) = core.node.start().await {
                tracing::error!("Failed to restart node: {}", e);
            }

            // Refresh chat context with new network info
            let address = core.wallet.get_primary_address().await
                .unwrap_or_else(|| "not connected".to_string());
            let height = core.node.get_status().await.block_height;
            core.chat.set_context(&address, "0", &lower, height).await;

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_environment(env_str.into());
                }
            });
        });
    });

    // =========================================================================
    // LEARNING CENTER WIRING — Join pool, stake, claim
    // =========================================================================

    // --- Learning: Join Pool ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_learning_join_pool(move || {
        tracing::info!("Learning: join pool requested");
        let core = core.clone();
        spawn_async(&rt_h, async move {
            match core.learning.list_pools().await {
                Ok(pools) => tracing::info!("Learning: {} pools available", pools.len()),
                Err(e) => tracing::error!("Learning: list pools failed: {}", e),
            }
        });
    });

    // --- Learning: Stake ---
    // Data source: LearningPool.joinPool(uint256) — sends SALT as msg.value
    // Contract: not yet deployed (address TBD from forge script output)
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_learning_stake(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Learning: stake requested");
        spawn_async(&rt_h, async move {
            // Check if pools exist
            match core.learning.list_pools().await {
                Ok(pools) if pools.is_empty() => {
                    tracing::info!("Learning: no pools available — contract not deployed");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_learning_pool_status("No pools — contracts not yet deployed".into());
                        }
                    });
                }
                Ok(pools) => {
                    tracing::info!("Learning: {} pools found, staking to first", pools.len());
                    // Once deployed: send joinPool(poolId) tx with stake value
                }
                Err(e) => tracing::error!("Learning: pool query failed: {}", e),
            }
        });
    });

    // --- Learning: Claim Earnings ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_learning_claim_earnings(move || {
        tracing::info!("Learning: claim earnings requested");
        let core = core.clone();
        spawn_async(&rt_h, async move {
            match core.learning.get_earnings("default").await {
                Ok(earnings) => tracing::info!("Learning: earnings = {} SALT", earnings),
                Err(e) => tracing::error!("Learning: get earnings failed: {}", e),
            }
        });
    });

    // =========================================================================
    // COMPUTE MARKETPLACE WIRING — Post job, register provider, refresh
    // =========================================================================

    // --- Compute: Post Job ---
    // Data source: ComputeMarketplace.postJob(bytes32,uint256,uint256) — sends tx
    // Contract: not yet deployed (address TBD from forge script output)
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_compute_post_job(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Compute: post job requested");
        spawn_async(&rt_h, async move {
            match core.compute.list_providers().await {
                Ok(providers) if providers.is_empty() => {
                    tracing::info!("Compute: no providers — contract not deployed");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_compute_provider_status("No providers — contracts not yet deployed".into());
                        }
                    });
                }
                Ok(providers) => {
                    tracing::info!("Compute: {} providers available", providers.len());
                    // Once deployed: send postJob(modelHash, budget, deadline) tx
                }
                Err(e) => tracing::error!("Compute: provider query failed: {}", e),
            }
        });
    });

    // --- Compute: Register Provider ---
    // Data source: ComputeMarketplace.registerProvider(string,string,uint32,uint256) — sends tx
    // Contract: not yet deployed (address TBD from forge script output)
    let core = app_core.clone();
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_compute_register_provider(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Compute: register provider requested");
        spawn_async(&rt_h, async move {
            match core.compute.list_providers().await {
                Ok(providers) => {
                    let count = providers.len() as i32;
                    tracing::info!("Compute: {} providers on-chain", count);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_compute_providers(count);
                            if count == 0 {
                                ui.set_compute_provider_status("Contracts not yet deployed".into());
                            }
                        }
                    });
                }
                Err(e) => tracing::error!("Compute: query failed: {}", e),
            }
        });
    });

    // --- Compute: Refresh ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_compute_refresh(move || {
        let core = core.clone();
        tracing::info!("Compute: refreshing");
        spawn_async(&rt_h, async move {
            match core.compute.list_jobs().await {
                Ok(jobs) => tracing::info!("Compute: {} jobs", jobs.len()),
                Err(e) => tracing::error!("Compute: refresh failed: {}", e),
            }
        });
    });

    // =========================================================================
    // CHAT TOOL APPROVAL WIRING
    // =========================================================================

    // --- Chat: Approve Tool ---
    let ui_w = ui.as_weak();
    ui.on_chat_approve_tool(move || {
        tracing::info!("Chat: tool approved by user");
        // Tool approval will execute the pending action when the agent
        // architecture is fully wired (FINAL-5). For now, log the approval.
        if let Some(ui) = ui_w.upgrade() {
            ui.set_chat_last_response("Tool approved. Executing...".into());
        }
    });

    // --- Chat: Reject Tool ---
    let ui_w = ui.as_weak();
    ui.on_chat_reject_tool(move || {
        tracing::info!("Chat: tool rejected by user");
        if let Some(ui) = ui_w.upgrade() {
            ui.set_chat_last_response("Tool action cancelled.".into());
        }
    });

    // =========================================================================
    // SETTINGS: AI Configuration, Model Scanning, Model Download
    // =========================================================================

    // --- Save AI API Key ---
    let core = app_core.clone();
    let rt_h = rt.handle().clone();
    ui.on_settings_save_ai_key(move |provider, key| {
        let provider_str = provider.to_string();
        let key_str = key.to_string();
        let core = core.clone();
        tracing::info!("Settings: saving API key for {} ({} chars)", provider_str, key_str.len());
        spawn_async(&rt_h, async move {
            let mut config = core.config.write().await;
            config.ai_keys.insert(provider_str.clone(), key_str);
            if let Err(e) = config.save() {
                tracing::error!("Failed to save config: {}", e);
            } else {
                tracing::info!("Settings: {} API key saved to disk", provider_str);
            }
        });
    });

    // --- Scan for Local Models (Ollama + GGUF) ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    let core = app_core.clone();
    ui.on_settings_scan_models(move || {
        let ui_w = ui_w.clone();
        let core = core.clone();
        tracing::info!("Settings: scanning for local AI backends (Ollama + GGUF)");
        spawn_async(&rt_h, async move {
            let detected = citrate_desktop_app::services::ChatService::detect_local_backend().await;
            let found = detected.backend_type != "none";
            let display_name = detected.display_name.clone();
            let model_id = detected.model_id.clone();
            tracing::info!("Settings: detected backend={}, model={}", detected.backend_type, display_name);

            // Update the chat service model so subsequent messages use the right model
            if found {
                core.chat.set_model(&model_id).await;
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_chat_model_loaded(found);
                    if found {
                        ui.set_chat_model_name(display_name.into());
                    }
                }
            });
        });
    });

    // --- Download Model (Qwen 2.5 1.5B Q4_0 ~1GB from HuggingFace) ---
    let ui_w = ui.as_weak();
    let rt_h = rt.handle().clone();
    ui.on_settings_download_model(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Settings: starting Qwen 2.5 1.5B download");
        spawn_async(&rt_h, async move {
            let model_dir = dirs::home_dir()
                .map(|d| d.join(".citrate/models"))
                .unwrap_or_else(|| std::path::PathBuf::from(".citrate/models"));
            if let Err(e) = std::fs::create_dir_all(&model_dir) {
                tracing::error!("Failed to create model dir: {}", e);
                return;
            }

            let model_path = model_dir.join("qwen2.5-1.5b-instruct-q4_0.gguf");
            if model_path.exists() {
                tracing::info!("Model already downloaded: {:?}", model_path);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_chat_model_loaded(true);
                        ui.set_chat_model_name("qwen2.5-1.5b-instruct-q4_0.gguf".into());
                    }
                });
                return;
            }

            let url = "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_0.gguf";
            tracing::info!("Downloading {} to {:?} (~1GB)", url, model_path);

            let client = reqwest::Client::new();
            match client.get(url).send().await {
                Ok(response) => {
                    if !response.status().is_success() {
                        tracing::error!("Download failed: HTTP {}", response.status());
                        return;
                    }
                    let total = response.content_length().unwrap_or(0);
                    tracing::info!("Download started: {} MB", total / 1024 / 1024);

                    // Stream to file to avoid holding 1GB in memory
                    use tokio::io::AsyncWriteExt;
                    let file = tokio::fs::File::create(&model_path).await;
                    match file {
                        Ok(mut file) => {
                            let mut stream = response.bytes_stream();
                            use futures_util::StreamExt;
                            let mut downloaded: u64 = 0;
                            while let Some(chunk) = stream.next().await {
                                match chunk {
                                    Ok(bytes) => {
                                        if let Err(e) = file.write_all(&bytes).await {
                                            tracing::error!("Write failed: {}", e);
                                            return;
                                        }
                                        downloaded += bytes.len() as u64;
                                        if downloaded % (50 * 1024 * 1024) < bytes.len() as u64 {
                                            tracing::info!("Downloaded {} / {} MB",
                                                downloaded / 1024 / 1024,
                                                total / 1024 / 1024);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Download stream error: {}", e);
                                        return;
                                    }
                                }
                            }
                            tracing::info!("Model download complete: {:?} ({} MB)",
                                model_path, downloaded / 1024 / 1024);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    ui.set_chat_model_loaded(true);
                                    ui.set_chat_model_name("qwen2.5-1.5b-instruct-q4_0.gguf".into());
                                }
                            });
                        }
                        Err(e) => tracing::error!("Failed to create file: {}", e),
                    }
                }
                Err(e) => tracing::error!("Download request failed: {}", e),
            }
        });
    });

    // =========================================================================
    // STORAGE (IPFS) WIRING
    // =========================================================================

    // Storage (IPFS) — connected to daemon via HTTP API on localhost:5001
    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_start_daemon(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: starting IPFS daemon");
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001 — check if already running
            let already_running = client
                .post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);

            if already_running {
                tracing::info!("Storage: IPFS daemon already running — updating UI");
                let stats = ipfs_fetch_stats(&client).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_storage_daemon_online(true);
                        ui.set_storage_peer_count(stats.peer_count);
                        ui.set_storage_pin_count(stats.pin_count);
                        ui.set_storage_repo_size(stats.repo_size.into());
                    }
                });
                return;
            }

            let config = citrate_storage::ipfs::DaemonConfig::default();
            let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
            match daemon.initialize().await {
                Ok(()) => {
                    tracing::info!("Storage: IPFS daemon started");
                    // Fetch live stats after startup
                    let stats = ipfs_fetch_stats(&client).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                Err(e) => tracing::error!("Storage: daemon failed: {}", e),
            }
        });
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_stop_daemon(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: stopping IPFS daemon");
        spawn_async(&rt_h, async move {
            let config = citrate_storage::ipfs::DaemonConfig::default();
            let daemon = citrate_storage::ipfs::IpfsDaemon::new(config);
            if let Err(e) = daemon.stop().await {
                tracing::warn!("Storage: stop failed: {}", e);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_storage_daemon_online(false);
                    ui.set_storage_peer_count(0);
                    ui.set_storage_pin_count(0);
                    ui.set_storage_repo_size("0 B".into());
                }
            });
        });
    });

    ui.on_storage_upload_file(move || {
        tracing::info!("Storage: upload file requested — file dialog not yet available in Slint");
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_pin_cid(move |cid| {
        let cid_str = cid.to_string();
        let ui_w = ui_w.clone();
        tracing::info!("Storage: pinning CID {}", cid_str);
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001/api/v0/pin/add
            match client.post("http://127.0.0.1:5001/api/v0/pin/add")
                .query(&[("arg", &cid_str)])
                .timeout(std::time::Duration::from_secs(30))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: pinned CID successfully");
                    // Refresh stats after successful pin
                    let stats = ipfs_fetch_stats(&client).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                Ok(resp) => tracing::warn!("Storage: pin failed — status {}", resp.status()),
                Err(e) => tracing::error!("Storage: pin failed — is IPFS running? {}", e),
            }
        });
    });

    let rt_h = rt.handle().clone();
    let ui_w = ui.as_weak();
    ui.on_storage_refresh(move || {
        let ui_w = ui_w.clone();
        tracing::info!("Storage: checking daemon status");
        spawn_async(&rt_h, async move {
            let client = reqwest::Client::new();
            // Data source: IPFS daemon HTTP API at localhost:5001
            match client.post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(3))
                .send().await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: IPFS daemon is online");
                    let stats = ipfs_fetch_stats(&client).await;
                    tracing::info!(
                        "Storage: {} peers, {} pins, repo {}",
                        stats.peer_count, stats.pin_count, stats.repo_size
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                _ => {
                    tracing::info!("Storage: IPFS daemon is offline");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(false);
                            ui.set_storage_peer_count(0);
                            ui.set_storage_pin_count(0);
                            ui.set_storage_repo_size("0 B".into());
                        }
                    });
                }
            }
        });
    });

    // =========================================================================
    // IPFS AUTO-DETECT ON STARTUP
    // =========================================================================
    // Data source: IPFS daemon HTTP API at localhost:5001 — auto-detect if already running
    {
        let ui_w = ui.as_weak();
        let rt_h = rt.handle().clone();
        rt_h.spawn(async move {
            let client = reqwest::Client::new();
            match client
                .post("http://127.0.0.1:5001/api/v0/id")
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Storage: auto-detected running IPFS daemon");
                    let stats = ipfs_fetch_stats(&client).await;
                    tracing::info!(
                        "Storage: auto-detect — {} peers, {} pins, repo {}",
                        stats.peer_count, stats.pin_count, stats.repo_size
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_storage_daemon_online(true);
                            ui.set_storage_peer_count(stats.peer_count);
                            ui.set_storage_pin_count(stats.pin_count);
                            ui.set_storage_repo_size(stats.repo_size.into());
                        }
                    });
                }
                _ => {
                    tracing::info!("Storage: no IPFS daemon detected at startup");
                }
            }
        });
    }

    tracing::info!("Citrate Desktop ready (full wiring)");
    ui.run().expect("Slint event loop failed");
}
