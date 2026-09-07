//! App binder — extracts callback wiring from main.rs into a reusable layer.
//!
//! Both the live app (main.rs) and the test harness call `bind_model_publish()`
//! to wire the same service orchestration. This ensures callback-driven tests
//! exercise the same code paths as the running application.

use crate::{App, spawn_async};
use citrate_desktop_app::AppCore;
use citrate_desktop_app::services::model_service::ModelPublishState;
use slint::ComponentHandle;
use std::sync::Arc;

/// Bind model publish callbacks: deploy-model triggers the full publish lifecycle
/// (hash → pin → submit → poll receipt → readback verify) through ModelService.
///
/// This is the same wiring used by main.rs. Tests can call this function
/// with a test AppCore to exercise the real publish pipeline.
pub fn bind_model_publish(
    app: &App,
    core: Arc<AppCore>,
    rt_handle: &tokio::runtime::Handle,
) {
    let ui_w = app.as_weak();
    let rt_h = rt_handle.clone();

    app.on_models_deploy_model(move || {
        let core = core.clone();
        let ui_w = ui_w.clone();
        tracing::info!("Models: deploying model to on-chain registry (via app_binder)");

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

            // 2. Compute model hash from actual file bytes (VM-1)
            let model_bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("Models: failed to read model file: {}", e);
                    return;
                }
            };
            let model_hash: [u8; 32] = {
                use sha3::{Digest, Keccak256};
                let mut hasher = Keccak256::new();
                hasher.update(&model_bytes);
                hasher.finalize().into()
            };
            let content_hash = hex::encode(model_hash);
            tracing::info!("Models: artifact hash={} size={} bytes",
                &content_hash[..16], model_bytes.len());

            // 3. Get sender address
            let from = match core.wallet.get_primary_address().await {
                Some(addr) => addr,
                None => {
                    tracing::error!("Models: no wallet address for deploy tx");
                    return;
                }
            };

            // 4. Init publish record in ModelService
            core.models.init_publish(
                &path.to_string_lossy(),
                &content_hash,
                model_bytes.len() as u64,
                &from,
            ).await;

            // 5. Get CID if pinned
            let cid = ui_w.upgrade()
                .map(|ui: App| ui.get_models_ipfs_cid().to_string())
                .filter(|s: &String| !s.is_empty())
                .unwrap_or_else(|| format!("hash:{}", hex::encode(model_hash)));

            if !cid.starts_with("hash:") {
                core.models.mark_pinned(&cid).await;
                let _ = slint::invoke_from_event_loop({
                    let ui_w = ui_w.clone();
                    move || { if let Some(ui) = ui_w.upgrade() { ui.set_models_publish_state("pinned".into()); } }
                });
            }

            // 6. Build registerModel calldata
            let calldata = encode_register_model_calldata(&model_hash, &cid);

            // 7. Send tx to model precompile
            let precompile = "0x0000000000000000000000000000000000001000";
            // NAT-B-007: surface the decoded registerModel intent + target for
            // explicit Approve/Deny before broadcasting to the precompile.
            if !crate::confirm_tx_intent(&core.approvals, "Publish model", precompile, "0", &calldata).await {
                let ui_w2 = ui_w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w2.upgrade() {
                        ui.set_models_publish_state("cancelled".into());
                    }
                });
                return;
            }
            match core.wallet.send_transaction_with_data(
                &from, precompile, "0", calldata, ""
            ).await {
                Ok(tx_hash) => {
                    tracing::info!("Models: deploy tx submitted: {}", tx_hash);
                    core.models.mark_submitted(&tx_hash).await;
                    let _ = slint::invoke_from_event_loop({
                        let ui_w = ui_w.clone();
                        let tx = tx_hash.clone();
                        move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_models_ipfs_cid(format!("tx: {}", tx).into());
                                ui.set_models_publish_state("submitted".into());
                            }
                        }
                    });

                    // VM-3: Poll for receipt
                    for _ in 0..15 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        match core.models.poll_receipt().await {
                            Ok(state) => {
                                let state_str = state.as_str().to_string();
                                let ui_w2 = ui_w.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_w2.upgrade() {
                                        ui.set_models_publish_state(state_str.into());
                                    }
                                });
                                if matches!(state,
                                    ModelPublishState::Confirmed | ModelPublishState::Failed
                                ) {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Models: receipt poll error: {}", e);
                                break;
                            }
                        }
                    }

                    // VM-4: Registry readback verification
                    if let Some(record) = core.models.publish_record().await {
                        if record.state == ModelPublishState::Confirmed {
                            match core.models.verify_readback().await {
                                Ok(state) => {
                                    let state_str = state.as_str().to_string();
                                    tracing::info!("Models: readback result: {}", state_str);
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_w.upgrade() {
                                            ui.set_models_publish_state(state_str.into());
                                        }
                                    });
                                }
                                Err(e) => tracing::warn!("Models: readback failed: {}", e),
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Models: deploy failed: {}", e);
                }
            }
        });
    });
}

/// ABI-encode `registerModel(bytes32,string)` calldata for the model
/// registry precompile.
///
/// GUI_NATIVE-2026-05-31-004 (WP 6.4b): extracted from the deploy closure so
/// the encoding is unit-testable. The string length word previously did
/// `len() as u8` — any CID/URI longer than 255 bytes silently truncated the
/// declared length to `len % 256`, producing corrupt calldata.
pub(crate) fn encode_register_model_calldata(model_hash: &[u8; 32], cid: &str) -> Vec<u8> {
    let selector: [u8; 4] = {
        use sha3::{Digest, Keccak256};
        let hash = Keccak256::digest(b"registerModel(bytes32,string)");
        [hash[0], hash[1], hash[2], hash[3]]
    };
    let cid_bytes = cid.as_bytes();
    let padded_len = cid_bytes.len().div_ceil(32) * 32;
    let mut calldata = Vec::with_capacity(4 + 32 + 32 + 32 + padded_len);
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(model_hash);
    let mut offset = [0u8; 32];
    offset[31] = 0x40;
    calldata.extend_from_slice(&offset);
    let mut len_bytes = [0u8; 32];
    // Full big-endian length — `len() as u8` truncated CIDs/URIs > 255 bytes.
    len_bytes[24..32].copy_from_slice(&(cid_bytes.len() as u64).to_be_bytes());
    calldata.extend_from_slice(&len_bytes);
    calldata.extend_from_slice(cid_bytes);
    calldata.resize(calldata.len() + padded_len - cid_bytes.len(), 0);
    calldata
}

#[cfg(test)]
mod tests {
    use super::encode_register_model_calldata;

    /// The string length word must carry the FULL big-endian length — a CID
    /// longer than 255 bytes must not wrap modulo 256 (the `as u8` bug).
    #[test]
    fn register_model_length_word_is_not_truncated() {
        let model_hash = [0x11u8; 32];
        let long_cid = "Q".repeat(300); // 300 > 255 → wrapped to 44 pre-fix
        let calldata = encode_register_model_calldata(&model_hash, &long_cid);

        let len_word = &calldata[4 + 64..4 + 96];
        let declared = u64::from_be_bytes(len_word[24..32].try_into().unwrap());
        assert!(len_word[..24].iter().all(|b| *b == 0));
        assert_eq!(declared, 300, "declared string length must be the real length");
        // Payload + zero padding to a word boundary.
        assert_eq!(calldata.len(), 4 + 32 + 32 + 32 + 320);
    }

    /// Short-CID layout stays canonical: selector, bytes32, offset 0x40,
    /// length, padded payload.
    #[test]
    fn register_model_layout_is_canonical() {
        let model_hash = [0xabu8; 32];
        let calldata = encode_register_model_calldata(&model_hash, "QmShortCid");
        assert_eq!(&calldata[4..36], &model_hash);
        assert_eq!(calldata[4 + 63], 0x40, "offset word");
        assert_eq!(calldata[4 + 95], 10, "length word");
        assert_eq!(&calldata[4 + 96..4 + 106], b"QmShortCid");
        assert_eq!(calldata.len(), 4 + 96 + 32, "payload padded to one word");
    }
}
