//! Typed event bus for background → UI communication.
//!
//! Background tasks (node polling, wallet refresh, model downloads) publish
//! typed events. The UI layer subscribes and updates view models.
//! No string payloads — every event has structure.

use tokio::sync::broadcast;

/// Events published by background services.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Node status changed (block height, peers, etc.)
    NodeStatusChanged {
        running: bool,
        block_height: u64,
        peer_count: u32,
        syncing: bool,
    },

    /// Wallet balance updated
    BalanceUpdated {
        address: String,
        balance_wei: String,
    },

    /// Block reward received
    RewardReceived {
        block_height: u64,
        amount_wei: String,
    },

    /// Transaction confirmed
    TransactionConfirmed {
        tx_hash: String,
        block_height: u64,
        success: bool,
    },

    /// Model download progress
    ModelDownloadProgress {
        model_name: String,
        progress_percent: f32,
        status: String,
    },

    /// Error occurred in background service
    BackgroundError {
        service: String,
        message: String,
    },

    /// Editor buffer content changed (needs re-render)
    EditorBufferChanged {
        buffer_id: String,
    },

    /// Terminal output ready (needs re-render)
    TerminalOutputReady {
        session_id: String,
    },

    /// Terminal session closed
    TerminalSessionClosed {
        session_id: String,
    },

    /// Git status changed
    GitStatusChanged {
        branch: String,
        changed_count: usize,
        staged_count: usize,
    },

    /// File tree changed (directory expanded/collapsed, file created/deleted)
    FileTreeChanged,

    /// File system watch event
    FileWatchEvent {
        path: String,
        event_type: String,
    },

    /// Compilation completed
    CompileCompleted {
        success: bool,
        error_count: usize,
        warning_count: usize,
    },
}

/// Broadcast-based event bus. Multiple subscribers can listen.
pub struct EventBus {
    sender: broadcast::Sender<AppEvent>,
}

impl EventBus {
    /// Create a new event bus with capacity for 256 buffered events.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }

    /// Publish an event to all subscribers.
    pub fn publish(&self, event: AppEvent) {
        // Ignore error if no subscribers
        let _ = self.sender.send(event);
    }

    /// Subscribe to events. Returns a receiver that yields events.
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus_publish_subscribe() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: 42,
            peer_count: 3,
            syncing: false,
        });

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::NodeStatusChanged { block_height, .. } => {
                assert_eq!(block_height, 42);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_event_bus_multiple_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(AppEvent::BalanceUpdated {
            address: "0xabc".to_string(),
            balance_wei: "1000".to_string(),
        });

        let e1 = rx1.recv().await.expect("event received");
        let e2 = rx2.recv().await.expect("event received");

        match (e1, e2) {
            (
                AppEvent::BalanceUpdated { address: a1, .. },
                AppEvent::BalanceUpdated { address: a2, .. },
            ) => {
                assert_eq!(a1, "0xabc");
                assert_eq!(a2, "0xabc");
            }
            _ => panic!("Both subscribers should get the same event"),
        }
    }

    #[test]
    fn test_publish_without_subscribers() {
        let bus = EventBus::new();
        // Should not panic even with no subscribers
        bus.publish(AppEvent::BackgroundError {
            service: "test".to_string(),
            message: "no one listening".to_string(),
        });
    }

    #[tokio::test]
    async fn test_event_ordering_preserved() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        for i in 0..10 {
            bus.publish(AppEvent::NodeStatusChanged {
                running: true,
                block_height: i,
                peer_count: 0,
                syncing: false,
            });
        }

        for i in 0..10 {
            let event = rx.recv().await.expect("event received");
            match event {
                AppEvent::NodeStatusChanged { block_height, .. } => {
                    assert_eq!(block_height, i);
                }
                _ => panic!("Wrong event type at index {}", i),
            }
        }
    }

    #[tokio::test]
    async fn test_mixed_event_types() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: 1,
            peer_count: 3,
            syncing: false,
        });
        bus.publish(AppEvent::BalanceUpdated {
            address: "0xabc".to_string(),
            balance_wei: "1000".to_string(),
        });
        bus.publish(AppEvent::RewardReceived {
            block_height: 5,
            amount_wei: "100".to_string(),
        });
        bus.publish(AppEvent::TransactionConfirmed {
            tx_hash: "0xhash".to_string(),
            block_height: 6,
            success: true,
        });
        bus.publish(AppEvent::ModelDownloadProgress {
            model_name: "qwen".to_string(),
            progress_percent: 50.0,
            status: "downloading".to_string(),
        });

        let mut count = 0;
        while count < 5 {
            let _ = rx.recv().await.expect("event received");
            count += 1;
        }
        assert_eq!(count, 5);
    }

    #[tokio::test]
    async fn test_late_subscriber_misses_early_events() {
        let bus = EventBus::new();

        // Publish before subscribing
        bus.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: 1,
            peer_count: 0,
            syncing: false,
        });

        let mut rx = bus.subscribe();

        // Publish after subscribing
        bus.publish(AppEvent::NodeStatusChanged {
            running: true,
            block_height: 2,
            peer_count: 0,
            syncing: false,
        });

        let event = rx.recv().await.expect("event received");
        match event {
            AppEvent::NodeStatusChanged { block_height, .. } => {
                assert_eq!(block_height, 2, "Late subscriber should only see event 2");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_event_bus_capacity() {
        let bus = EventBus::new();
        let _rx = bus.subscribe();

        // Publish 256 events (buffer capacity) — should not panic
        for i in 0..256 {
            bus.publish(AppEvent::NodeStatusChanged {
                running: true,
                block_height: i,
                peer_count: 0,
                syncing: false,
            });
        }
    }

    #[tokio::test]
    async fn test_three_subscribers_all_receive() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let mut rx3 = bus.subscribe();

        bus.publish(AppEvent::RewardReceived {
            block_height: 42,
            amount_wei: "1000000".to_string(),
        });

        for rx in [&mut rx1, &mut rx2, &mut rx3] {
            let event = rx.recv().await.expect("event received");
            match event {
                AppEvent::RewardReceived { block_height, .. } => assert_eq!(block_height, 42),
                _ => panic!("Wrong event type"),
            }
        }
    }

    #[test]
    fn test_event_debug_format() {
        let event = AppEvent::BackgroundError {
            service: "node".to_string(),
            message: "connection lost".to_string(),
        };
        let debug = format!("{:?}", event);
        assert!(debug.contains("BackgroundError"));
        assert!(debug.contains("node"));
    }

    #[test]
    fn test_event_clone() {
        let event = AppEvent::TransactionConfirmed {
            tx_hash: "0xabc".to_string(),
            block_height: 100,
            success: true,
        };
        let cloned = event.clone();
        match cloned {
            AppEvent::TransactionConfirmed { tx_hash, block_height, success } => {
                assert_eq!(tx_hash, "0xabc");
                assert_eq!(block_height, 100);
                assert!(success);
            }
            _ => panic!("Clone changed type"),
        }
    }

    #[test]
    fn test_all_event_variants_constructible() {
        let events: [AppEvent; 6] = [
            AppEvent::NodeStatusChanged { running: true, block_height: 0, peer_count: 0, syncing: false },
            AppEvent::BalanceUpdated { address: "0x".into(), balance_wei: "0".into() },
            AppEvent::RewardReceived { block_height: 0, amount_wei: "0".into() },
            AppEvent::TransactionConfirmed { tx_hash: "0x".into(), block_height: 0, success: false },
            AppEvent::ModelDownloadProgress { model_name: "m".into(), progress_percent: 0.0, status: "s".into() },
            AppEvent::BackgroundError { service: "s".into(), message: "m".into() },
        ];
        assert_eq!(events.len(), 6);
    }
}
