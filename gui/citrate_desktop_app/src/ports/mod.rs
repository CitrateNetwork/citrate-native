//! Port interfaces — abstractions over infrastructure dependencies.
//!
//! The desktop app core defines ports (traits) that infrastructure
//! adapters implement. This allows testing services without real
//! RocksDB, network, or filesystem dependencies.

/// Port for persistent key-value storage
pub trait KeyValueStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
    fn delete(&self, key: &str);
}

/// Port for secure secret storage (API keys, wallet passwords)
pub trait SecretStore: Send + Sync {
    fn get_secret(&self, key: &str) -> Option<String>;
    fn set_secret(&self, key: &str, value: &str) -> Result<(), String>;
    fn delete_secret(&self, key: &str) -> Result<(), String>;
    fn has_secret(&self, key: &str) -> bool;
}

/// Production secret store backed by the operating system credential store.
#[derive(Debug, Clone, Default)]
pub struct SystemSecretStore;

impl SystemSecretStore {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn keyring_entry(key: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new("citrate-desktop", key)
        .map_err(|e| format!("OS keychain entry error for {key}: {e}"))
}

impl SecretStore for SystemSecretStore {
    fn get_secret(&self, key: &str) -> Option<String> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = keyring_entry(key).ok()?;
            match entry.get_password() {
                Ok(secret) => Some(secret),
                Err(keyring::Error::NoEntry) => None,
                Err(e) => {
                    tracing::warn!("OS keychain read failed for {}: {}", key, e);
                    None
                }
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = key;
            None
        }
    }

    fn set_secret(&self, key: &str, value: &str) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = keyring_entry(key)?;
            entry
                .set_password(value)
                .map_err(|e| format!("OS keychain write failed for {key}: {e}"))
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (key, value);
            Err("OS keychain is unsupported on this platform".to_string())
        }
    }

    fn delete_secret(&self, key: &str) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = keyring_entry(key)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!("OS keychain delete failed for {key}: {e}")),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = key;
            Err("OS keychain is unsupported on this platform".to_string())
        }
    }

    fn has_secret(&self, key: &str) -> bool {
        self.get_secret(key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory KV store for testing
    struct InMemoryKV {
        data: Mutex<HashMap<String, String>>,
    }

    impl InMemoryKV {
        fn new() -> Self {
            Self { data: Mutex::new(HashMap::new()) }
        }
    }

    impl KeyValueStore for InMemoryKV {
        fn get(&self, key: &str) -> Option<String> {
            self.data.lock().expect("mutex not poisoned").get(key).cloned()
        }
        fn set(&self, key: &str, value: &str) {
            self.data.lock().expect("mutex not poisoned").insert(key.to_string(), value.to_string());
        }
        fn delete(&self, key: &str) {
            self.data.lock().expect("mutex not poisoned").remove(key);
        }
    }

    /// In-memory secret store for testing
    struct InMemorySecrets {
        data: Mutex<HashMap<String, String>>,
    }

    impl InMemorySecrets {
        fn new() -> Self {
            Self { data: Mutex::new(HashMap::new()) }
        }
    }

    impl SecretStore for InMemorySecrets {
        fn get_secret(&self, key: &str) -> Option<String> {
            self.data.lock().expect("mutex not poisoned").get(key).cloned()
        }
        fn set_secret(&self, key: &str, value: &str) -> Result<(), String> {
            self.data.lock().expect("mutex not poisoned").insert(key.to_string(), value.to_string());
            Ok(())
        }
        fn delete_secret(&self, key: &str) -> Result<(), String> {
            self.data.lock().expect("mutex not poisoned").remove(key);
            Ok(())
        }
        fn has_secret(&self, key: &str) -> bool {
            self.data.lock().expect("mutex not poisoned").contains_key(key)
        }
    }

    // --- KeyValueStore tests ---

    #[test]
    fn test_kv_get_missing_key() {
        let store = InMemoryKV::new();
        assert!(store.get("missing").is_none());
    }

    #[test]
    fn test_kv_set_and_get() {
        let store = InMemoryKV::new();
        store.set("key1", "value1");
        assert_eq!(store.get("key1").expect("test assertion"), "value1");
    }

    #[test]
    fn test_kv_overwrite() {
        let store = InMemoryKV::new();
        store.set("key1", "v1");
        store.set("key1", "v2");
        assert_eq!(store.get("key1").expect("test assertion"), "v2");
    }

    #[test]
    fn test_kv_delete() {
        let store = InMemoryKV::new();
        store.set("key1", "value1");
        store.delete("key1");
        assert!(store.get("key1").is_none());
    }

    #[test]
    fn test_kv_delete_missing_key() {
        let store = InMemoryKV::new();
        // Should not panic
        store.delete("nonexistent");
    }

    #[test]
    fn test_kv_multiple_keys() {
        let store = InMemoryKV::new();
        for i in 0..100 {
            store.set(&format!("key{}", i), &format!("value{}", i));
        }
        for i in 0..100 {
            assert_eq!(
                store.get(&format!("key{}", i)).expect("test assertion"),
                format!("value{}", i)
            );
        }
    }

    #[test]
    fn test_kv_empty_key() {
        let store = InMemoryKV::new();
        store.set("", "empty_key_value");
        assert_eq!(store.get("").expect("test assertion"), "empty_key_value");
    }

    #[test]
    fn test_kv_empty_value() {
        let store = InMemoryKV::new();
        store.set("key", "");
        assert_eq!(store.get("key").expect("test assertion"), "");
    }

    #[test]
    fn test_kv_unicode_keys() {
        let store = InMemoryKV::new();
        store.set("ключ", "значение");
        assert_eq!(store.get("ключ").expect("test assertion"), "значение");
    }

    #[test]
    fn test_kv_long_value() {
        let store = InMemoryKV::new();
        let long_val = "x".repeat(10_000);
        store.set("key", &long_val);
        assert_eq!(store.get("key").expect("test assertion").len(), 10_000);
    }

    // --- SecretStore tests ---

    #[test]
    fn test_secret_get_missing() {
        let store = InMemorySecrets::new();
        assert!(store.get_secret("missing").is_none());
    }

    #[test]
    fn test_secret_set_and_get() {
        let store = InMemorySecrets::new();
        store.set_secret("api_key", "sk-12345").expect("test assertion");
        assert_eq!(store.get_secret("api_key").expect("test assertion"), "sk-12345");
    }

    #[test]
    fn test_secret_has_secret() {
        let store = InMemorySecrets::new();
        assert!(!store.has_secret("key"));
        store.set_secret("key", "value").expect("test assertion");
        assert!(store.has_secret("key"));
    }

    #[test]
    fn test_secret_delete() {
        let store = InMemorySecrets::new();
        store.set_secret("key", "value").expect("test assertion");
        store.delete_secret("key").expect("test assertion");
        assert!(!store.has_secret("key"));
        assert!(store.get_secret("key").is_none());
    }

    #[test]
    fn test_secret_delete_missing() {
        let store = InMemorySecrets::new();
        let result = store.delete_secret("nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn test_secret_overwrite() {
        let store = InMemorySecrets::new();
        store.set_secret("key", "v1").expect("test assertion");
        store.set_secret("key", "v2").expect("test assertion");
        assert_eq!(store.get_secret("key").expect("test assertion"), "v2");
    }

    #[test]
    fn test_secret_multiple() {
        let store = InMemorySecrets::new();
        store.set_secret("wallet_password", "pass123").expect("test assertion");
        store.set_secret("api_key", "key456").expect("test assertion");
        store.set_secret("rpc_token", "tok789").expect("test assertion");
        assert!(store.has_secret("wallet_password"));
        assert!(store.has_secret("api_key"));
        assert!(store.has_secret("rpc_token"));
        assert!(!store.has_secret("nonexistent"));
    }

    // --- Trait object tests ---

    #[test]
    fn test_kv_as_trait_object() {
        let store: Box<dyn KeyValueStore> = Box::new(InMemoryKV::new());
        store.set("key", "value");
        assert_eq!(store.get("key").expect("test assertion"), "value");
    }

    #[test]
    fn test_secret_as_trait_object() {
        let store: Box<dyn SecretStore> = Box::new(InMemorySecrets::new());
        store.set_secret("key", "secret").expect("test assertion");
        assert!(store.has_secret("key"));
    }

    #[test]
    fn test_kv_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<InMemoryKV>();
        assert_sync::<InMemoryKV>();
    }

    #[test]
    fn test_secret_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<InMemorySecrets>();
        assert_sync::<InMemorySecrets>();
    }
}
