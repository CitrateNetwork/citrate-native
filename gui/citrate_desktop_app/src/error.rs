//! Application-level errors with typed variants.
//!
//! UI code matches on these variants to show appropriate error surfaces.
//! No `Result<String, String>` — every error has structure.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Node error: {0}")]
    Node(String),

    #[error("Wallet error: {0}")]
    Wallet(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Chain query failed: {0}")]
    ChainQuery(String),

    #[error("Contract call failed: {method} on {contract}: {reason}")]
    ContractCall {
        contract: String,
        method: String,
        reason: String,
    },

    #[error("Insufficient funds: have {have}, need {need}")]
    InsufficientFunds { have: String, need: String },

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Session expired")]
    SessionExpired,

    #[error("Rate limited: {0}")]
    RateLimited(String),

    #[error("Model not loaded: {0}")]
    ModelNotLoaded(String),

    #[error("Editor error: {0}")]
    Editor(String),

    #[error("Terminal error: {0}")]
    Terminal(String),

    #[error("Git error: {0}")]
    Git(String),

    #[error("Compiler error: {0}")]
    Compiler(String),

    #[error("File system error: {0}")]
    FileSystem(String),

    #[error("Buffer not found: {0}")]
    BufferNotFound(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = AppError::InsufficientFunds {
            have: "1.5 SALT".to_string(),
            need: "10.0 SALT".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Insufficient funds: have 1.5 SALT, need 10.0 SALT"
        );
    }

    #[test]
    fn test_contract_error_display() {
        let err = AppError::ContractCall {
            contract: "ModelRegistry".to_string(),
            method: "getModel".to_string(),
            reason: "not deployed".to_string(),
        };
        assert!(err.to_string().contains("ModelRegistry"));
        assert!(err.to_string().contains("getModel"));
    }

    #[test]
    fn test_error_variants_exist() {
        // Ensure all error variants compile and display
        let errors: Vec<AppError> = vec![
            AppError::Node("test".into()),
            AppError::Wallet("test".into()),
            AppError::Storage("test".into()),
            AppError::Network("test".into()),
            AppError::Config("test".into()),
            AppError::ChainQuery("test".into()),
            AppError::SessionExpired,
            AppError::RateLimited("24h".into()),
            AppError::ModelNotLoaded("qwen".into()),
            AppError::InvalidAddress("0xbad".into()),
            AppError::Editor("test".into()),
            AppError::Terminal("test".into()),
            AppError::Git("test".into()),
            AppError::Compiler("test".into()),
            AppError::FileSystem("test".into()),
            AppError::BufferNotFound("buf-1".into()),
            AppError::SessionNotFound("sess-1".into()),
        ];
        for err in &errors {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn test_node_error_display() {
        let err = AppError::Node("connection refused".to_string());
        assert_eq!(err.to_string(), "Node error: connection refused");
    }

    #[test]
    fn test_wallet_error_display() {
        let err = AppError::Wallet("decryption failed".to_string());
        assert_eq!(err.to_string(), "Wallet error: decryption failed");
    }

    #[test]
    fn test_storage_error_display() {
        let err = AppError::Storage("disk full".to_string());
        assert_eq!(err.to_string(), "Storage error: disk full");
    }

    #[test]
    fn test_network_error_display() {
        let err = AppError::Network("timeout".to_string());
        assert_eq!(err.to_string(), "Network error: timeout");
    }

    #[test]
    fn test_config_error_display() {
        let err = AppError::Config("invalid chain_id".to_string());
        assert_eq!(err.to_string(), "Configuration error: invalid chain_id");
    }

    #[test]
    fn test_chain_query_error_display() {
        let err = AppError::ChainQuery("block not found".to_string());
        assert_eq!(err.to_string(), "Chain query failed: block not found");
    }

    #[test]
    fn test_session_expired_display() {
        let err = AppError::SessionExpired;
        assert_eq!(err.to_string(), "Session expired");
    }

    #[test]
    fn test_rate_limited_display() {
        let err = AppError::RateLimited("retry after 60s".to_string());
        assert_eq!(err.to_string(), "Rate limited: retry after 60s");
    }

    #[test]
    fn test_model_not_loaded_display() {
        let err = AppError::ModelNotLoaded("llama-7b".to_string());
        assert_eq!(err.to_string(), "Model not loaded: llama-7b");
    }

    #[test]
    fn test_invalid_address_display() {
        let err = AppError::InvalidAddress("0xZZZ".to_string());
        assert_eq!(err.to_string(), "Invalid address: 0xZZZ");
    }

    #[test]
    fn test_contract_call_error_fields() {
        let err = AppError::ContractCall {
            contract: "StakingPool".to_string(),
            method: "stake".to_string(),
            reason: "insufficient allowance".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("StakingPool"));
        assert!(msg.contains("stake"));
        assert!(msg.contains("insufficient allowance"));
    }

    #[test]
    fn test_insufficient_funds_fields() {
        let err = AppError::InsufficientFunds {
            have: "0.5 SALT".to_string(),
            need: "100.0 SALT".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("0.5 SALT"));
        assert!(msg.contains("100.0 SALT"));
    }

    #[test]
    fn test_error_debug_format() {
        let err = AppError::SessionExpired;
        let debug = format!("{:?}", err);
        assert!(debug.contains("SessionExpired"));
    }

    #[test]
    fn test_internal_error_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("something broke internally");
        let app_err = AppError::Internal(anyhow_err);
        assert!(app_err.to_string().contains("something broke internally"));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<AppError>();
        assert_sync::<AppError>();
    }

    #[test]
    fn test_empty_string_errors() {
        let errors = vec![
            AppError::Node("".to_string()),
            AppError::Wallet("".to_string()),
            AppError::Network("".to_string()),
        ];
        for err in errors {
            // Even with empty inner string, Display should not panic
            let _ = err.to_string();
        }
    }

    #[test]
    fn test_contract_call_with_empty_fields() {
        let err = AppError::ContractCall {
            contract: "".to_string(),
            method: "".to_string(),
            reason: "".to_string(),
        };
        // Should not panic
        let _ = err.to_string();
    }
}
