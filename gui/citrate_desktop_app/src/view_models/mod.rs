//! View model definitions consumed by the UI layer.
//!
//! View models are the contract between the desktop app core and the
//! presentation layer (Slint). They expose:
//! - Properties for render state
//! - Typed callbacks for user intent
//! - List models for repeating data
//!
//! View models do NOT expose raw Result<String, String> or IPC command names.

pub mod ide_view_models;

/// Shell view model — sidebar, status bar, tab state
pub struct ShellViewModel {
    pub active_tab: String,
    pub node_running: bool,
    pub block_height: u64,
    pub peer_count: u32,
    pub environment: String,
    pub chain_id: u64,
}

/// Onboarding view model — step state machine
pub struct OnboardingViewModel {
    pub current_step: u32,
    pub total_steps: u32,
    pub persona: Option<String>,       // home, teacher, developer
    pub wallet_created: bool,
    pub mnemonic: Option<String>,      // shown once
    pub error: Option<String>,
}

/// Wallet view model — account list and operations
pub struct WalletViewModel {
    pub accounts: Vec<AccountViewModel>,
    pub selected_address: Option<String>,
    pub session_active: bool,
    pub total_balance: String,
}

/// Single account for display
pub struct AccountViewModel {
    pub address: String,
    pub label: String,
    pub balance_display: String,  // "123.4567 SALT"
    pub is_default: bool,
}

/// DAG explorer view model
pub struct DagViewModel {
    pub blocks: Vec<BlockViewModel>,
    pub height: u64,
    pub loading: bool,
    pub error: Option<String>,
}

/// Single block for display
pub struct BlockViewModel {
    pub hash_short: String,       // "0xb3af...2a6a"
    pub height: u64,
    pub timestamp_display: String, // "2:34:56 PM"
    pub tx_count: usize,
    pub selected_parent_short: String,
}

/// Settings view model
pub struct SettingsViewModel {
    pub network: String,
    pub chain_id: u64,
    pub rpc_port: u16,
    pub p2p_port: u16,
    pub bootnodes: Vec<String>,
    pub theme: String,
    pub node_running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_view_model_construction() {
        let vm = ShellViewModel {
            active_tab: "dashboard".to_string(),
            node_running: true,
            block_height: 42,
            peer_count: 3,
            environment: "DEVNET".to_string(),
            chain_id: 40204,
        };
        assert_eq!(vm.active_tab, "dashboard");
        assert!(vm.node_running);
        assert_eq!(vm.block_height, 42);
        assert_eq!(vm.peer_count, 3);
        assert_eq!(vm.chain_id, 40204);
    }

    #[test]
    fn test_onboarding_view_model_defaults() {
        let vm = OnboardingViewModel {
            current_step: 0,
            total_steps: 10,
            persona: None,
            wallet_created: false,
            mnemonic: None,
            error: None,
        };
        assert_eq!(vm.current_step, 0);
        assert_eq!(vm.total_steps, 10);
        assert!(vm.persona.is_none());
        assert!(!vm.wallet_created);
        assert!(vm.mnemonic.is_none());
        assert!(vm.error.is_none());
    }

    #[test]
    fn test_onboarding_view_model_with_persona() {
        let vm = OnboardingViewModel {
            current_step: 4,
            total_steps: 10,
            persona: Some("developer".to_string()),
            wallet_created: true,
            mnemonic: Some("word1 word2 word3".to_string()),
            error: None,
        };
        assert_eq!(vm.persona.expect("test assertion"), "developer");
        assert!(vm.wallet_created);
    }

    #[test]
    fn test_onboarding_view_model_with_error() {
        let vm = OnboardingViewModel {
            current_step: 1,
            total_steps: 10,
            persona: None,
            wallet_created: false,
            mnemonic: None,
            error: Some("Password too short".to_string()),
        };
        assert!(vm.error.is_some());
        assert!(vm.error.expect("test assertion").contains("Password"));
    }

    #[test]
    fn test_wallet_view_model_empty() {
        let vm = WalletViewModel {
            accounts: vec![],
            selected_address: None,
            session_active: false,
            total_balance: "0".to_string(),
        };
        assert!(vm.accounts.is_empty());
        assert!(vm.selected_address.is_none());
        assert!(!vm.session_active);
    }

    #[test]
    fn test_wallet_view_model_with_accounts() {
        let vm = WalletViewModel {
            accounts: vec![
                AccountViewModel {
                    address: "0xabc".to_string(),
                    label: "Primary".to_string(),
                    balance_display: "100.0000 SALT".to_string(),
                    is_default: true,
                },
                AccountViewModel {
                    address: "0xdef".to_string(),
                    label: "Secondary".to_string(),
                    balance_display: "50.0000 SALT".to_string(),
                    is_default: false,
                },
            ],
            selected_address: Some("0xabc".to_string()),
            session_active: true,
            total_balance: "150.0000 SALT".to_string(),
        };
        assert_eq!(vm.accounts.len(), 2);
        assert!(vm.accounts[0].is_default);
        assert!(!vm.accounts[1].is_default);
        assert!(vm.session_active);
    }

    #[test]
    fn test_account_view_model_fields() {
        let vm = AccountViewModel {
            address: "0x9f5B156C53305D4b20c94ca08E3219D1C0e7401a".to_string(),
            label: "Deployer".to_string(),
            balance_display: "999999.0000 SALT".to_string(),
            is_default: false,
        };
        assert!(vm.address.starts_with("0x"));
        assert_eq!(vm.label, "Deployer");
        assert!(vm.balance_display.contains("SALT"));
    }

    #[test]
    fn test_dag_view_model_empty() {
        let vm = DagViewModel {
            blocks: vec![],
            height: 0,
            loading: false,
            error: None,
        };
        assert!(vm.blocks.is_empty());
        assert_eq!(vm.height, 0);
        assert!(!vm.loading);
    }

    #[test]
    fn test_dag_view_model_loading() {
        let vm = DagViewModel {
            blocks: vec![],
            height: 0,
            loading: true,
            error: None,
        };
        assert!(vm.loading);
    }

    #[test]
    fn test_dag_view_model_with_error() {
        let vm = DagViewModel {
            blocks: vec![],
            height: 100,
            loading: false,
            error: Some("RPC timeout".to_string()),
        };
        assert!(vm.error.is_some());
    }

    #[test]
    fn test_dag_view_model_with_blocks() {
        let vm = DagViewModel {
            blocks: vec![
                BlockViewModel {
                    hash_short: "0xb3af...2a6a".to_string(),
                    height: 100,
                    timestamp_display: "2:34:56 PM".to_string(),
                    tx_count: 5,
                    selected_parent_short: "0xd1ee...ff32".to_string(),
                },
            ],
            height: 100,
            loading: false,
            error: None,
        };
        assert_eq!(vm.blocks.len(), 1);
        assert_eq!(vm.blocks[0].height, 100);
        assert_eq!(vm.blocks[0].tx_count, 5);
    }

    #[test]
    fn test_block_view_model_fields() {
        let vm = BlockViewModel {
            hash_short: "0xabcd...ef01".to_string(),
            height: 42,
            timestamp_display: "12:00:00 AM".to_string(),
            tx_count: 0,
            selected_parent_short: "0x0000...0000".to_string(),
        };
        assert!(vm.hash_short.starts_with("0x"));
        assert_eq!(vm.height, 42);
        assert_eq!(vm.tx_count, 0);
    }

    #[test]
    fn test_settings_view_model_defaults() {
        let vm = SettingsViewModel {
            network: "devnet".to_string(),
            chain_id: 40204,
            rpc_port: 18545,
            p2p_port: 30304,
            bootnodes: vec!["node1@1.2.3.4:30303".to_string()],
            theme: "dark".to_string(),
            node_running: false,
        };
        assert_eq!(vm.chain_id, 40204);
        assert_eq!(vm.rpc_port, 18545);
        assert_ne!(vm.rpc_port, vm.p2p_port);
        assert!(!vm.node_running);
    }

    #[test]
    fn test_settings_view_model_light_theme() {
        let vm = SettingsViewModel {
            network: "testnet".to_string(),
            chain_id: 40204,
            rpc_port: 18545,
            p2p_port: 30304,
            bootnodes: vec![],
            theme: "light".to_string(),
            node_running: true,
        };
        assert_eq!(vm.theme, "light");
        assert!(vm.node_running);
    }

    #[test]
    fn test_persona_variants() {
        for persona in ["home", "teacher", "developer"] {
            let vm = OnboardingViewModel {
                current_step: 4,
                total_steps: 10,
                persona: Some(persona.to_string()),
                wallet_created: false,
                mnemonic: None,
                error: None,
            };
            assert_eq!(vm.persona.expect("test assertion"), persona);
        }
    }

    #[test]
    fn test_onboarding_step_range() {
        for step in 0..=10 {
            let vm = OnboardingViewModel {
                current_step: step,
                total_steps: 10,
                persona: None,
                wallet_created: false,
                mnemonic: None,
                error: None,
            };
            assert!(vm.current_step <= vm.total_steps);
        }
    }
}
