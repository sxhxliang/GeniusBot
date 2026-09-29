//! One event stream for the console: host events plus server events
//! (model-call log updates, settings changes), numbered and timestamped.
//! Recent non-streaming events are kept for clients that connect late.

use gns_core::HostEvent;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// An event as sent to the console.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BusEvent {
    pub seq: u64,
    pub ts: i64,
    /// `host` (a [`HostEvent`]) or `server`.
    pub channel: &'static str,
    /// The host event's `type`, or the server event name.
    pub kind: String,
    pub data: serde_json::Value,
}

/// Kinds too chatty to keep in the recent-events buffer.
const TRANSIENT_KINDS: &[&str] = &["text-delta", "thinking-delta", "llm-log"];

#[derive(Debug)]
struct Inner {
    tx: broadcast::Sender<Arc<BusEvent>>,
    recent: Mutex<VecDeque<Arc<BusEvent>>>,
    capacity: usize,
    seq: AtomicU64,
}

/// Cheap to clone; all clones share one stream.
#[derive(Clone, Debug)]
pub struct EventBus(Arc<Inner>);

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self(Arc::new(Inner { tx, recent: Mutex::new(VecDeque::new()), capacity: capacity.max(1), seq: AtomicU64::new(1) }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<BusEvent>> {
        self.0.tx.subscribe()
    }

    /// Recent events, oldest first, optionally only those after `after_seq`.
    pub fn recent(&self, after_seq: Option<u64>, limit: usize) -> Vec<Arc<BusEvent>> {
        let recent = self.0.recent.lock().expect("event lock");
        let matching: Vec<_> = recent.iter().filter(|e| after_seq.is_none_or(|s| e.seq > s)).cloned().collect();
        matching[matching.len().saturating_sub(limit)..].to_vec()
    }

    pub fn publish_host(&self, event: &HostEvent) {
        let data = serde_json::to_value(event).unwrap_or_default();
        let kind = data["type"].as_str().unwrap_or("unknown").to_owned();
        self.publish("host", kind, data);
    }

    pub fn publish_server(&self, kind: &str, data: serde_json::Value) {
        self.publish("server", kind.to_owned(), data);
    }

    fn publish(&self, channel: &'static str, kind: String, data: serde_json::Value) {
        let event =
            Arc::new(BusEvent { seq: self.0.seq.fetch_add(1, Ordering::Relaxed), ts: gns_core::text::now_ms(), channel, kind, data });
        if !TRANSIENT_KINDS.contains(&event.kind.as_str()) {
            let mut recent = self.0.recent.lock().expect("event lock");
            recent.push_back(event.clone());
            while recent.len() > self.0.capacity {
                recent.pop_front();
            }
        }
        let _ = self.0.tx.send(event);
    }

    /// Forward the host's events until it shuts down.
    pub fn forward(&self, mut rx: broadcast::Receiver<HostEvent>) -> tokio::task::JoinHandle<()> {
        let bus = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => bus.publish_host(&event),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        bus.publish_server("lagged", serde_json::json!({ "skipped": n }));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_skips_transient_events_and_honours_the_cursor() {
        let bus = EventBus::new(2);
        bus.publish_server("model-changed", serde_json::json!({}));
        bus.publish_server("llm-log", serde_json::json!({}));
        bus.publish_server("a", serde_json::json!({}));
        bus.publish_server("b", serde_json::json!({}));
        let kinds: Vec<_> = bus.recent(None, 10).iter().map(|e| e.kind.clone()).collect();
        assert_eq!(kinds, ["a", "b"]);
        let after: Vec<_> = bus.recent(Some(3), 10).iter().map(|e| e.kind.clone()).collect();
        assert_eq!(after, ["b"]);
    }
}
