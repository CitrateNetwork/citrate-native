//! Trail layer — converts runtime events into canonical TrailEvents
//! and projects them to LogSeq journal pages.
//!
//! Architecture: TrailEvent is the machine truth. LogSeq pages are the human
//! truth projection. Chain anchors are the public integrity proof.
//! See .agentile/audits/2026-03/2026-03-31-agent-ecosystem-strategy/ for rationale.

use crate::event_bus::{AppEvent, EventBus};
use citrate_agent_core::canonical::{TrailEvent, LogseqProjection};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Trail subscriber — listens to the event bus and records TrailEvents.
pub struct TrailRecorder {
    /// All trail events in the current session (append-only)
    events: Arc<RwLock<Vec<TrailEvent>>>,
    /// Session ID for correlation
    session_id: String,
    /// LogSeq graph path (if enabled)
    logseq_path: Option<String>,
}

impl TrailRecorder {
    pub fn new(session_id: &str, logseq_path: Option<String>) -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
            session_id: session_id.to_string(),
            logseq_path,
        }
    }

    /// Get the LogSeq graph path (if configured)
    pub async fn logseq_path(&self) -> Option<String> {
        self.logseq_path.clone()
    }

    /// Record a trail event
    pub async fn record(&self, event: TrailEvent) {
        tracing::debug!("Trail: {} — {}", event.event_type, event.tool_name.as_deref().unwrap_or(""));
        self.events.write().await.push(event);
    }

    /// Convert an AppEvent to a TrailEvent and record it
    pub async fn record_app_event(&self, app_event: &AppEvent) {
        let now = chrono::Utc::now().to_rfc3339();
        let trail_event = match app_event {
            AppEvent::TransactionConfirmed { tx_hash, block_height, success } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "transaction_confirmed".to_string(),
                    tool_name: Some("send_tx".to_string()),
                    data: serde_json::json!({
                        "tx_hash": tx_hash,
                        "block_height": block_height,
                        "success": success,
                    }),
                    risk_level: Some("high".to_string()),
                    approved: Some(true),
                    duration_ms: None,
                }
            }
            AppEvent::NodeStatusChanged { running, block_height, peer_count, syncing } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "node_status".to_string(),
                    tool_name: None,
                    data: serde_json::json!({
                        "running": running,
                        "block_height": block_height,
                        "peer_count": peer_count,
                        "syncing": syncing,
                    }),
                    risk_level: None,
                    approved: None,
                    duration_ms: None,
                }
            }
            AppEvent::BalanceUpdated { address, balance_wei } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "balance_updated".to_string(),
                    tool_name: None,
                    data: serde_json::json!({
                        "address": address,
                        "balance_wei": balance_wei,
                    }),
                    risk_level: None,
                    approved: None,
                    duration_ms: None,
                }
            }
            AppEvent::ToolCallRequested { tool_name, risk_level, target } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "tool_call_requested".to_string(),
                    tool_name: Some(tool_name.clone()),
                    data: serde_json::json!({
                        "risk_level": risk_level,
                        "target": target,
                    }),
                    risk_level: Some(risk_level.clone()),
                    approved: None,
                    duration_ms: None,
                }
            }
            AppEvent::ToolCallApproved { tool_name, request_id } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "tool_call_approved".to_string(),
                    tool_name: Some(tool_name.clone()),
                    data: serde_json::json!({ "request_id": request_id }),
                    risk_level: Some("high".to_string()),
                    approved: Some(true),
                    duration_ms: None,
                }
            }
            AppEvent::ToolCallDenied { tool_name, request_id } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "tool_call_denied".to_string(),
                    tool_name: Some(tool_name.clone()),
                    data: serde_json::json!({ "request_id": request_id }),
                    risk_level: Some("high".to_string()),
                    approved: Some(false),
                    duration_ms: None,
                }
            }
            AppEvent::ToolCallCompleted { tool_name, success, duration_ms, result_summary } => {
                TrailEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: self.session_id.clone(),
                    timestamp: now,
                    event_type: "tool_call_completed".to_string(),
                    tool_name: Some(tool_name.clone()),
                    data: serde_json::json!({
                        "success": success,
                        "result_summary": result_summary,
                    }),
                    risk_level: Some("high".to_string()),
                    approved: Some(true),
                    duration_ms: Some(*duration_ms),
                }
            }
            _ => return, // Other events don't need trail recording yet
        };
        self.record(trail_event).await;
    }

    /// Get all events in this session
    pub async fn get_events(&self) -> Vec<TrailEvent> {
        self.events.read().await.clone()
    }

    /// Get event count
    pub async fn event_count(&self) -> usize {
        self.events.read().await.len()
    }

    /// Generate a LogSeq journal page from today's trail events.
    /// Writes to `{logseq_path}/journals/YYYY_MM_DD.md`.
    pub async fn write_logseq_journal(&self) -> Result<Option<LogseqProjection>, std::io::Error> {
        let path = match &self.logseq_path {
            Some(p) => p.clone(),
            None => return Ok(None),
        };

        let events = self.events.read().await;
        if events.is_empty() {
            return Ok(None);
        }

        let today = chrono::Utc::now().format("%Y_%m_%d").to_string();
        let journal_dir = std::path::Path::new(&path).join("journals");
        std::fs::create_dir_all(&journal_dir)?;

        let mut content = String::new();
        content.push_str(&format!("- **Citrate Agent Session** ({})\n", self.session_id));
        content.push_str(&format!("  - Events: {}\n", events.len()));

        for event in events.iter() {
            let tool = event.tool_name.as_deref().unwrap_or("system");
            let risk = event.risk_level.as_deref().unwrap_or("—");
            content.push_str(&format!(
                "  - `{}` {} (risk: {}) {}\n",
                event.timestamp.split('T').next_back().unwrap_or(&event.timestamp),
                event.event_type,
                risk,
                tool,
            ));
        }

        let journal_path = journal_dir.join(format!("{}.md", today));

        // Append to existing journal or create new
        let existing = std::fs::read_to_string(&journal_path).unwrap_or_default();
        let full_content = if existing.is_empty() {
            content.clone()
        } else {
            format!("{}\n{}", existing, content)
        };
        std::fs::write(&journal_path, &full_content)?;

        tracing::info!("Trail: wrote LogSeq journal to {:?} ({} events)", journal_path, events.len());

        let event_ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();

        Ok(Some(LogseqProjection {
            title: format!("Citrate Session {}", today),
            page_type: "journal".to_string(),
            content,
            content_hash: None,
            source_event_ids: event_ids,
        }))
    }

    /// Start a background task that subscribes to the event bus
    /// and records trail events automatically.
    pub fn start_subscriber(
        self: Arc<Self>,
        events: Arc<EventBus>,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = events.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                self.record_app_event(&event).await;
            }
            tracing::info!("Trail: event subscriber stopped");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_trail_event() {
        let recorder = TrailRecorder::new("test-session", None);
        let event = TrailEvent {
            id: "evt-1".to_string(),
            session_id: "test-session".to_string(),
            timestamp: "2026-03-31T10:00:00Z".to_string(),
            event_type: "tool_call".to_string(),
            tool_name: Some("check_balance".to_string()),
            data: serde_json::json!({}),
            risk_level: Some("low".to_string()),
            approved: Some(true),
            duration_ms: Some(100),
        };
        recorder.record(event).await;
        assert_eq!(recorder.event_count().await, 1);
    }

    #[tokio::test]
    async fn test_record_app_event_tx() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::TransactionConfirmed {
            tx_hash: "0xabc".to_string(),
            block_height: 100,
            success: true,
        };
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 1);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "transaction_confirmed");
    }

    #[tokio::test]
    async fn test_non_trail_events_ignored() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::FileTreeChanged;
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 0);
    }

    #[tokio::test]
    async fn test_logseq_journal_without_path() {
        let recorder = TrailRecorder::new("test-session", None);
        let result = recorder.write_logseq_journal().await.expect("no error");
        assert!(result.is_none()); // No logseq path configured
    }

    #[tokio::test]
    async fn test_record_tool_call_requested() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::ToolCallRequested {
            tool_name: "send_tx".to_string(),
            risk_level: "high".to_string(),
            target: "0xabc".to_string(),
        };
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 1);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "tool_call_requested");
        assert_eq!(events[0].risk_level.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn test_record_tool_call_approved() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::ToolCallApproved {
            tool_name: "send_tx".to_string(),
            request_id: "req-123".to_string(),
        };
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 1);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "tool_call_approved");
        assert_eq!(events[0].approved, Some(true));
    }

    #[tokio::test]
    async fn test_record_tool_call_denied() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::ToolCallDenied {
            tool_name: "deploy_contract".to_string(),
            request_id: "req-456".to_string(),
        };
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 1);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "tool_call_denied");
        assert_eq!(events[0].approved, Some(false));
    }

    #[tokio::test]
    async fn test_record_tool_call_completed() {
        let recorder = TrailRecorder::new("test-session", None);
        let app_event = AppEvent::ToolCallCompleted {
            tool_name: "send_tx".to_string(),
            success: true,
            duration_ms: 1500,
            result_summary: "tx 0xabc sent".to_string(),
        };
        recorder.record_app_event(&app_event).await;
        assert_eq!(recorder.event_count().await, 1);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "tool_call_completed");
        assert_eq!(events[0].duration_ms, Some(1500));
    }

    #[tokio::test]
    async fn test_full_tool_lifecycle_trail() {
        let recorder = TrailRecorder::new("test-session", None);
        // Simulate full lifecycle: request → approve → complete
        recorder.record_app_event(&AppEvent::ToolCallRequested {
            tool_name: "send_tx".to_string(),
            risk_level: "high".to_string(),
            target: "0xdead".to_string(),
        }).await;
        recorder.record_app_event(&AppEvent::ToolCallApproved {
            tool_name: "send_tx".to_string(),
            request_id: "req-789".to_string(),
        }).await;
        recorder.record_app_event(&AppEvent::ToolCallCompleted {
            tool_name: "send_tx".to_string(),
            success: true,
            duration_ms: 2000,
            result_summary: "Transaction sent".to_string(),
        }).await;
        assert_eq!(recorder.event_count().await, 3);
        let events = recorder.get_events().await;
        assert_eq!(events[0].event_type, "tool_call_requested");
        assert_eq!(events[1].event_type, "tool_call_approved");
        assert_eq!(events[2].event_type, "tool_call_completed");
    }
}
