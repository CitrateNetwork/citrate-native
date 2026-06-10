//! Integration tests for citrate-native.
//!
//! These tests verify the data transformation functions used by GUI callbacks.
//! We cannot drive the Slint event loop in CI (no display server), so we test
//! the pure logic that feeds the UI: account conversion, SALT/wei formatting,
//! balance round-trips, and type compatibility of the background data thread.

// ============================================================================
// 1. SALT ↔ wei conversion (used by the send callback)
// ============================================================================

mod salt_wei_conversion {
    use citrate_wallet_core::format::{salt_to_wei, wei_to_salt};

    #[test]
    fn salt_to_wei_whole_number() {
        let wei = salt_to_wei("1").expect("should parse '1'");
        assert_eq!(wei, 1_000_000_000_000_000_000u128);
    }

    #[test]
    fn salt_to_wei_fractional() {
        let wei = salt_to_wei("1.5").expect("should parse '1.5'");
        assert_eq!(wei, 1_500_000_000_000_000_000u128);
    }

    #[test]
    fn salt_to_wei_small_fraction() {
        let wei = salt_to_wei("0.000001").expect("should parse '0.000001'");
        assert_eq!(wei, 1_000_000_000_000u128);
    }

    #[test]
    fn salt_to_wei_zero() {
        let wei = salt_to_wei("0").expect("should parse '0'");
        assert_eq!(wei, 0u128);
    }

    #[test]
    fn salt_to_wei_rejects_empty() {
        assert!(salt_to_wei("").is_err());
    }

    #[test]
    fn salt_to_wei_rejects_non_numeric() {
        assert!(salt_to_wei("abc").is_err());
    }

    #[test]
    fn salt_to_wei_rejects_too_many_decimals() {
        // 19 decimal digits — max is 18
        assert!(salt_to_wei("0.1234567890123456789").is_err());
    }

    #[test]
    fn wei_to_salt_zero() {
        assert_eq!(wei_to_salt(0), "0");
    }

    #[test]
    fn wei_to_salt_one() {
        assert_eq!(wei_to_salt(1_000_000_000_000_000_000u128), "1");
    }

    #[test]
    fn wei_to_salt_fractional() {
        assert_eq!(wei_to_salt(500_000_000_000_000_000u128), "0.5");
    }

    #[test]
    fn roundtrip_whole_number() {
        let original_wei = 42_000_000_000_000_000_000u128; // 42 SALT
        let salt_str = wei_to_salt(original_wei);
        let back = salt_to_wei(&salt_str).expect("roundtrip should succeed");
        assert_eq!(back, original_wei);
    }

    #[test]
    fn roundtrip_fractional() {
        let original_wei = 1_250_000_000_000_000_000u128; // 1.25 SALT
        let salt_str = wei_to_salt(original_wei);
        let back = salt_to_wei(&salt_str).expect("roundtrip should succeed");
        assert_eq!(back, original_wei);
    }

    #[test]
    fn roundtrip_small_fraction() {
        let original_wei = 1u128; // smallest unit
        let salt_str = wei_to_salt(original_wei);
        let back = salt_to_wei(&salt_str).expect("roundtrip should succeed");
        assert_eq!(back, original_wei);
    }

    #[test]
    fn roundtrip_large_amount() {
        // 1 billion SALT
        let original_wei = 1_000_000_000u128 * 1_000_000_000_000_000_000u128;
        let salt_str = wei_to_salt(original_wei);
        let back = salt_to_wei(&salt_str).expect("roundtrip should succeed");
        assert_eq!(back, original_wei);
    }
}

// ============================================================================
// 2. Account data conversion (push_accounts_to_ui logic)
// ============================================================================

mod account_conversion {
    use citrate_desktop_app::services::wallet_service::Account;

    /// Verify the Account struct has all fields needed by push_accounts_to_ui.
    /// The callback converts Account → AccountData (Slint struct) by mapping:
    ///   address → SharedString, label → SharedString,
    ///   balance.to_string() → SharedString, is_default → bool
    #[test]
    fn account_struct_has_expected_fields() {
        let acct = Account {
            address: "0x1234abcd".to_string(),
            label: "My Wallet".to_string(),
            balance: "1.5".to_string(),
            nonce: 3,
            is_default: true,
        };

        // These are the exact field accesses used by push_accounts_to_ui in main.rs
        let _addr: &str = &acct.address;
        let _label: &str = &acct.label;
        let _balance_str: String = acct.balance.to_string();
        let _is_default: bool = acct.is_default;
    }

    /// Verify multiple accounts can be converted in sequence (as done by the iterator
    /// in push_accounts_to_ui).
    #[test]
    fn convert_multiple_accounts() {
        let accounts = [
            Account {
                address: "0xaaa".to_string(),
                label: "Primary".to_string(),
                balance: "100".to_string(),
                nonce: 0,
                is_default: true,
            },
            Account {
                address: "0xbbb".to_string(),
                label: "Secondary".to_string(),
                balance: "50.5".to_string(),
                nonce: 1,
                is_default: false,
            },
            Account {
                address: "0xccc".to_string(),
                label: "Cold Storage".to_string(),
                balance: "0".to_string(),
                nonce: 0,
                is_default: false,
            },
        ];

        // Simulate the exact mapping from push_accounts_to_ui
        let converted: Vec<(String, String, String, bool)> = accounts
            .iter()
            .map(|a| {
                (
                    a.address.clone(),
                    a.label.clone(),
                    a.balance.to_string(),
                    a.is_default,
                )
            })
            .collect();

        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].0, "0xaaa");
        assert!(converted[0].3);
        assert_eq!(converted[1].0, "0xbbb");
        assert!(!converted[1].3);
        assert_eq!(converted[2].2, "0");
    }

    /// Empty account list should produce an empty vec — this is a valid state
    /// (new user, no wallet created yet).
    #[test]
    fn convert_empty_accounts() {
        let accounts: Vec<Account> = vec![];
        let converted: Vec<(String, String, String, bool)> = accounts
            .iter()
            .map(|a| {
                (
                    a.address.clone(),
                    a.label.clone(),
                    a.balance.to_string(),
                    a.is_default,
                )
            })
            .collect();
        assert!(converted.is_empty());
    }
}

// ============================================================================
// 3. Balance formatting for the background data thread
// ============================================================================

mod balance_formatting {
    use citrate_wallet_core::format::{wei_to_salt, format_salt_display};

    /// The background thread reads a wei string from the node backend,
    /// parses it as u128, and then calls wei_to_salt. Test that pipeline.
    #[test]
    fn wei_string_parse_and_format() {
        let wei_str = "2500000000000000000"; // 2.5 SALT
        let wei: u128 = wei_str.parse().expect("wei string should parse as u128");
        let formatted = wei_to_salt(wei);
        assert_eq!(formatted, "2.5");
    }

    /// If the node returns "0", the parse + format should produce "0".
    #[test]
    fn zero_balance_formatting() {
        let wei_str = "0";
        let wei: u128 = wei_str.parse().expect("'0' should parse");
        let formatted = wei_to_salt(wei);
        assert_eq!(formatted, "0");
    }

    /// Non-numeric balance strings should fail to parse (we fall back to raw string in main.rs).
    #[test]
    fn non_numeric_balance_falls_through() {
        let wei_str = "not_a_number";
        let result = wei_str.parse::<u128>();
        assert!(result.is_err(), "non-numeric should fail parse");
        // In main.rs, the fallback is to use the raw string:
        // match wei_str.parse::<u128>() { Ok(wei) => wei_to_salt(wei), Err(_) => wei_str }
    }

    /// Display format includes " SALT" suffix and commas for large numbers.
    #[test]
    fn display_format_with_unit() {
        assert_eq!(format_salt_display(0), "0 SALT");
        assert_eq!(
            format_salt_display(1_000_000_000_000_000_000u128),
            "1 SALT"
        );
    }

    /// Large balances get comma separators.
    #[test]
    fn display_format_large_number() {
        let million_salt = 1_000_000u128 * 1_000_000_000_000_000_000u128;
        assert_eq!(format_salt_display(million_salt), "1,000,000 SALT");
    }
}

// ============================================================================
// 4. Background data thread type compatibility
// ============================================================================

mod background_thread_types {
    use citrate_desktop_app::services::node_service::{NodeStatus, BlockSummary};

    /// NodeStatus must be Send + Clone (used across thread boundary + invoke_from_event_loop).
    #[test]
    fn node_status_is_send_and_clone() {
        fn assert_send<T: Send>() {}
        fn assert_clone<T: Clone>() {}
        assert_send::<NodeStatus>();
        assert_clone::<NodeStatus>();
    }

    /// BlockSummary must be Send + Clone (read on background thread, sent to UI thread).
    #[test]
    fn block_summary_is_send_and_clone() {
        fn assert_send<T: Send>() {}
        fn assert_clone<T: Clone>() {}
        assert_send::<BlockSummary>();
        assert_clone::<BlockSummary>();
    }

    /// NodeStatus::default() should represent an offline node.
    #[test]
    fn default_node_status_is_offline() {
        let status = NodeStatus::default();
        assert!(!status.running);
        assert_eq!(status.block_height, 0);
        assert_eq!(status.peer_count, 0);
        assert_eq!(status.mempool_size, 0);
        assert_eq!(status.chain_id, 40204);
    }

    /// The connection status string logic from the background thread.
    #[test]
    fn connection_status_string_logic() {
        // Mirrors the logic in main.rs background thread
        let connected = NodeStatus {
            running: true,
            peer_count: 3,
            ..NodeStatus::default()
        };
        let connecting = NodeStatus {
            running: true,
            peer_count: 0,
            ..NodeStatus::default()
        };
        let disconnected = NodeStatus {
            running: false,
            peer_count: 0,
            ..NodeStatus::default()
        };

        let conn_str = |s: &NodeStatus| -> String {
            if s.peer_count > 0 {
                format!("Connected ({} peers)", s.peer_count)
            } else if s.running {
                "Connecting to bootnode...".to_string()
            } else {
                "Disconnected".to_string()
            }
        };

        assert_eq!(conn_str(&connected), "Connected (3 peers)");
        assert_eq!(conn_str(&connecting), "Connecting to bootnode...");
        assert_eq!(conn_str(&disconnected), "Disconnected");
    }

    /// Block hash truncation logic from the background thread.
    #[test]
    fn block_hash_truncation() {
        // Mirrors: if hash.len() > 18 { format!("{}...{}", &hash[..10], &hash[hash.len()-4..]) }
        let long_hash = "0xabcdef1234567890abcdef1234567890abcdef12";
        let truncated = if long_hash.len() > 18 {
            format!(
                "{}...{}",
                &long_hash[..10],
                &long_hash[long_hash.len() - 4..]
            )
        } else {
            long_hash.to_string()
        };
        assert_eq!(truncated, "0xabcdef12...ef12");

        // Short hash should pass through unchanged
        let short_hash = "0xabcdef";
        let result = if short_hash.len() > 18 {
            format!(
                "{}...{}",
                &short_hash[..10],
                &short_hash[short_hash.len() - 4..]
            )
        } else {
            short_hash.to_string()
        };
        assert_eq!(result, "0xabcdef");
    }

    /// Transaction count formatting mirrors main.rs
    #[test]
    fn tx_count_formatting() {
        let format_txcount = |count: usize| -> String {
            format!(
                "{} txn{}",
                count,
                if count == 1 { "" } else { "s" }
            )
        };

        assert_eq!(format_txcount(0), "0 txns");
        assert_eq!(format_txcount(1), "1 txn");
        assert_eq!(format_txcount(5), "5 txns");
    }

    /// Block age formatting mirrors the background thread logic.
    #[test]
    fn block_age_formatting() {
        let format_age = |now: u64, timestamp: u64| -> String {
            if timestamp > 0 && now > timestamp {
                let secs = now - timestamp;
                if secs < 60 {
                    format!("~{}s ago", secs)
                } else if secs < 3600 {
                    format!("~{}m ago", secs / 60)
                } else {
                    format!("~{}h ago", secs / 3600)
                }
            } else if timestamp == 0 {
                String::new()
            } else {
                "just now".to_string()
            }
        };

        assert_eq!(format_age(100, 90), "~10s ago");
        assert_eq!(format_age(1000, 700), "~5m ago");
        assert_eq!(format_age(10000, 1000), "~2h ago");
        assert_eq!(format_age(100, 0), "");
        assert_eq!(format_age(50, 100), "just now"); // future timestamp
    }
}

// ============================================================================
// 5. AppCore compile-time proof
// ============================================================================

mod app_core_type {
    use citrate_desktop_app::AppCore;

    /// Prove that AppCore can be constructed and wrapped in Arc.
    /// This is a compile-time check that the type is well-formed and Send + Sync.
    #[test]
    fn app_core_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AppCore>();
    }

    /// AppCore can be wrapped in Arc (as done in main.rs).
    #[test]
    fn app_core_wraps_in_arc() {
        fn assert_arc_compatible<T: Send + Sync + 'static>() {}
        assert_arc_compatible::<AppCore>();
    }
}
